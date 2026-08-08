//! The typed tree: lower `Analysis` + the lossless AST into a tree that carries
//! an inferred [`Type`](praxis_types::Type) on every node (ADR-014).
//!
//! Why this exists: [`Analysis`](crate::Analysis) records its types in side
//! tables keyed by range and node. The JIT backend needs the type of every node
//! — literals, binops, calls, … — inline, without re-running unification. This
//! pass re-walks the AST in "read mode" against the finalized [`TypeDb`]: each
//! node's type is **read** from what inference recorded (`expr_types`,
//! `ref_types`, `method_refs`), never re-derived and never unified for. A
//! construct it cannot lower becomes a `Y0xx`/`Y1xx` diagnostic in the returned
//! module so the CLI never feeds malformed input to the JIT.
//!
//! `analyze` is a pure input here — lowering only consumes its output (and grows
//! the type arena).

#![allow(dead_code)] // The typed tree is consumed by praxis-mir; not every
                     // variant is matched inside this crate.

use std::collections::HashMap;

use praxis_ast::{
    ArgList, AssignStmt, AstNode, BinExpr, BlockExpr, BreakExpr, CallExpr, ContinueExpr,
    ElseBranch, EnumItem, Expr, ExprStmt, FieldExpr, FnItem, ForExpr, IfExpr, Literal, LoopExpr,
    MethodCallExpr, Param, ParamList, PathExpr, RecordLitExpr, ReturnExpr, SourceFile, StructItem,
    TupleExpr, UnaryExpr, VarStmt, WhileExpr,
};
use praxis_source::{DiagCode, Diagnostic, FileSpan, Severity};
use praxis_stdlib::TypePattern;
use praxis_syntax::{span_bridge::range_to_span, SyntaxKind, SyntaxNode};
use praxis_types::{Type, TypeDb};
use rowan::TextRange;

use crate::{Analysis, ResolvedRef, ScopeTree, SymbolId};

// ---------------------------------------------------------------------------
// The typed tree
// ---------------------------------------------------------------------------

/// A whole lowered file: its top-level items and any lowering diagnostics.
#[derive(Debug)]
pub struct TypedModule {
    /// The top-level function declarations in source order, followed by the
    /// synthetic [`ENTRY_NAME`] function when the file has top-level statements.
    pub items: Vec<TypedItem>,
    /// The diagnostics lowering itself emits — a node inference recorded no type
    /// for (`Y099`), a field read on a concrete type that has none (`Y112`), and
    /// the reports the parser-expression lowering and `pattern::PatternBuilder`
    /// push through it. Empty for a fully lowerable module.
    pub diagnostics: Vec<Diagnostic>,
    /// The reassigned symbols captured by some closure in the module (escape
    /// analysis). The MIR builder boxes these into a `VarCell` at their binding
    /// site and routes reads/writes through the cell, so a mutation in one frame
    /// is visible to every closure sharing the cell.
    ///
    /// A subset of [`reassigned_vars`](Self::reassigned_vars): a capture that is
    /// never written needs no shared cell, only a copy.
    pub escaping_vars: std::collections::HashSet<SymbolId>,
    /// Every symbol some `name = …` statement writes (see
    /// [`Symbol::reassigned`](crate::Symbol::reassigned)).
    ///
    /// The MIR builder needs it wherever a binding would otherwise be an
    /// *alias* rather than a slot of its own — a match arm's `Bind` binds the
    /// scrutinee's local directly, and since every binding is assignable
    /// (ADR-125) a write through that name would land in whatever the
    /// scrutinee's local belongs to, which for a plain `match v { … }` is `v`.
    pub reassigned_vars: std::collections::HashSet<SymbolId>,
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
    /// The function's source span `[start, end)` as byte offsets (§9.3).
    /// Threaded through to MIR `Function` so the crash debugger's `source`
    /// command can render the faulting function's extent. `(0, 0)` only when the
    /// AST node has no usable span.
    pub span: (u32, u32),
}

/// A parameter `name: Type`.
#[derive(Clone, Debug)]
pub struct TypedParam {
    pub symbol: SymbolId,
    /// The name the programmer wrote, or `None` for a **destructuring**
    /// parameter (`|(a, b)| …`), whose slot holds the whole argument and which
    /// the programmer never named — the names in `(a, b)` are the components',
    /// bound in the body by `destructure_pattern_params`.
    ///
    /// `None` rather than the pattern's source text: MIR classifies a named
    /// parameter as a user binding and an unnamed one as a compiler temp, so
    /// pattern text would make the crash snapshot list a binding literally
    /// called `(a, b)` beside the `a` and `b` the source does have (ADR-139).
    pub name: Option<String>,
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
    /// `var name = expr` — the language's one binding form (ADR-125).
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
    /// `m[key] = v` / `counts[key] += 1` — a store through a subscript (§6.2).
    ///
    /// Its own statement rather than a desugaring into `m[key] = m[key] + v`: the
    /// desugared form names the receiver and every index **twice**, and MIR lowers
    /// each `TypedExpr` where it stands, so `m[f()] += 1` would call `f` twice.
    /// Carrying the pieces once is what makes the read-modify-write evaluate its
    /// place exactly once.
    ///
    /// `get` is the read symbol a compound operator needs and is `None` for a
    /// plain `=`, which reads nothing.
    IndexAssign {
        receiver: TypedExpr,
        indices: Vec<TypedExpr>,
        get: Option<praxis_stdlib::abi::RuntimeSymbol>,
        set: praxis_stdlib::abi::RuntimeSymbol,
        op: AssignOp,
        value: TypedExpr,
        span: (u32, u32),
    },
    /// `p.x = 5` / `p.x += 1` — a store into a record field (§4.5).
    ///
    /// Its own statement beside [`IndexAssign`](Self::IndexAssign) rather than a
    /// third catalog row, for the reason a field read is not a method call
    /// (ADR-077): the field is chosen by *name against one record definition* and
    /// lowers to a slot index, where a subscript is dispatched on the receiver's
    /// shape and arity. `field_idx` is that slot, read from the record's
    /// `RecordDef` exactly as [`TypedExpr::FieldGet`] reads it.
    ///
    /// The receiver is carried **once**, and a compound operator reads and writes
    /// through the one local MIR lowers it into — so `nodes(i).count += 1`
    /// evaluates `nodes(i)` once, which is the whole reason `IndexAssign` is not
    /// a desugaring either.
    FieldAssign {
        receiver: TypedExpr,
        field_idx: u32,
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
    /// A literal value — see [`Lit`] for the kinds, which include the
    /// synthesized `Unit`.
    Lit {
        value: Lit,
        ty: Type,
        span: (u32, u32),
    },
    /// An interpolated text literal: `"a{x}b{y}c"` (§8.1, ADR-147). `ty` is
    /// always `Text`.
    ///
    /// `parts` pairs each hole with the literal text that comes **before** it,
    /// and `trailing` is the text after the last hole — so `"a{x}b{y}c"` is
    /// `[("a", x), ("b", y)]` with `trailing = "c"`, and an empty `parts` cannot
    /// happen (an interpolated literal has at least one hole; a literal with none
    /// is an ordinary [`Lit`](Self::Lit)).
    ///
    /// The shape is "text before each hole, plus a tail" rather than a list of
    /// alternating pieces because that makes the invariant structural: there is
    /// exactly one fragment per hole and exactly one left over, so no consumer
    /// can lose a fragment or emit two in a row. The lowering folds it left with
    /// `praxis_text_concat`, and a piece list would have made the fold's
    /// correctness a property of the list rather than of the type.
    Interp {
        parts: Vec<(String, TypedExpr)>,
        trailing: String,
        ty: Type,
        span: (u32, u32),
    },
    /// A name reference (variable, parameter, function).
    Path {
        /// [`SymbolId::UNRESOLVED`] when resolution found no declaration for
        /// the name — lowering still emits the node so the rest of the file
        /// keeps being checked, and the diagnostic comes from resolution.
        symbol: SymbolId,
        ty: Type,
        span: (u32, u32),
    },
    /// A top-level `fn` name used in **value** position (ADR-061): `var f =
    /// double`, or `double` passed where a `Func` is expected.
    ///
    /// Distinct from [`Path`](Self::Path) because a `fn` is not a binding: it has
    /// no local slot, so there is no value for a `Path` to read and an indirect
    /// call through one has no function pointer. It lowers to a closure over the
    /// function with an empty environment (a top-level `fn` captures nothing).
    ///
    /// `callee_name` is here for the reason `Call`'s is: the MIR builder names
    /// its target without a `NameTable`.
    FnValue {
        callee: SymbolId,
        callee_name: String,
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
    /// `a..b` / `a..=b` (§4.11, ADR-059). `ty` is the nullary `Range`
    /// collection.
    ///
    /// `inclusive` is kept rather than normalized here: the bound is an
    /// arbitrary expression, so "add one to the end" is an *operation* and not a
    /// rewrite, and doing it in MIR keeps the overflow of `a..=Int::MAX` in one
    /// place instead of two.
    Range {
        start: Box<TypedExpr>,
        end: Box<TypedExpr>,
        inclusive: bool,
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
    /// `for binding in iter { body }` (§4.11). `binding` is the loop
    /// variable's **pattern** — a `Bind` for the ordinary `for x in …` and a
    /// composite for `for (k, v) in …` — and `item_ty` is the iterator's
    /// element type. Yields Unit.
    ///
    /// The pattern is irrefutable: a `for` has no second arm for an item to fall
    /// through to, and one that can fail is `Y125`, reported by
    /// `pattern::check_binding_patterns` at the end of analysis.
    For {
        binding: TypedPattern,
        iter: Box<TypedExpr>,
        body: Box<TypedBlock>,
        item_ty: Type,
        ty: Type,
        span: (u32, u32),
    },
    /// `loop { body }` (§4.11). An infinite loop terminated by `break`;
    /// its type is the break-value type (Unit if no break carries a value).
    Loop {
        body: Box<TypedBlock>,
        ty: Type,
        span: (u32, u32),
    },
    /// `break [expr]` (§4.11). Diverges from the enclosing loop. `value` is
    /// the optional break value; `ty` is `Never`.
    Break {
        value: Option<Box<TypedExpr>>,
        ty: Type,
        span: (u32, u32),
    },
    /// `continue` (§4.11). Diverges; `ty` is `Never`.
    Continue { ty: Type, span: (u32, u32) },
    /// `return [expr]` (§4.11). Diverges from the enclosing function.
    Return {
        value: Option<Box<TypedExpr>>,
        ty: Type,
        span: (u32, u32),
    },
    /// `callee(args)`.
    Call {
        /// The declaration the callee name resolves to, or
        /// [`SymbolId::UNRESOLVED`] when it resolves to none — which is also
        /// the case for every `callee_expr` call, since those have no callee
        /// *name* to resolve. A consumer must check `callee_expr` first rather
        /// than dispatching on this: an unresolved callee that carries a
        /// `callee_expr` is an indirect call, and a direct call emitted for it
        /// (`fs.get(0)(100)`) would call through nothing.
        callee: SymbolId,
        /// The callee's source name, resolved during HIR lowering so the MIR
        /// builder (and the JIT) can name the target without a NameTable.
        /// Empty when the call has no named callee at all; a named callee that
        /// merely failed to resolve still records its text.
        callee_name: String,
        args: Vec<TypedExpr>,
        /// The concrete argument types at this call site (§13.6). The mono pass
        /// uses these to instantiate a polymorphic callee; the MIR builder
        /// ignores them (calls are by name). For a closure-value callee, empty.
        arg_types: Vec<Type>,
        /// For a postfix call on an arbitrary expression (`expr(args)`, §4.10)
        /// — e.g. calling a closure retrieved from a collection
        /// (`fs.get(0)(100)`) or the result of another call (`f(1)(2)`) — the
        /// lowered callee expression. `None` for an ordinary named call (the
        /// callee is `callee`/`callee_name`); `Some` for a closure-value callee
        /// that the MIR builder lowers to `Inst::CallIndirect`.
        callee_expr: Option<Box<TypedExpr>>,
        ty: Type,
        span: (u32, u32),
    },
    /// `receiver.method(args)` (§16.2). `lowering_symbol` is the runtime wrapper
    /// the catalog resolved (e.g. `RuntimeSymbol::VecPush`), so the MIR builder
    /// emits a direct call without re-resolving the catalog; `None` means the
    /// method is an intrinsic and MIR lowers it itself. `purity` is the
    /// catalog's purity tag, so the crash debugger's read-only `p EXPR`
    /// evaluator can reject impure calls (§9.5, §19.10 "no command can mutate").
    MethodCall {
        receiver: Box<TypedExpr>,
        name: String,
        lowering_symbol: Option<praxis_stdlib::abi::RuntimeSymbol>,
        /// Whether the resolved row's receiver is the generic
        /// [`TypePattern::Iterable`](praxis_stdlib::TypePattern::Iterable)
        /// (ADR-127).
        ///
        /// Only a row that is *also* a `lowering_symbol` reads it, and what it
        /// says is: the wrapper needs a real `Vec` and the receiver may be any
        /// of ten things, so MIR materializes it first. Carried here rather than
        /// kept as a list of barrier symbols in the MIR builder, which would be a
        /// second statement of a fact the catalog already holds — and the kind
        /// that goes stale the day an eleventh row is added.
        receiver_is_iterable: bool,
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
    /// `[ a, b, … ]` — a `Vec` literal (§6.1). `ty` is the `Vec[T]` it builds;
    /// `elements` are the lowered element expressions in source order, possibly
    /// none (`[]`).
    ///
    /// Its own variant rather than a desugaring to `Vec()` + `push` calls in
    /// HIR: a `Call` needs a `SymbolId` for its callee and a `MethodCall` needs a
    /// catalog row instantiated at the element type, so the desugaring would have
    /// to synthesize name-resolution results that no name in the program produced
    /// — and every one of them would then be a thing the crash debugger, the
    /// purity gate and the escape walk see and have to explain. MIR builds the
    /// same two instructions from this variant without any of that.
    ListLit {
        elements: Vec<TypedExpr>,
        ty: Type,
        span: (u32, u32),
    },
    /// `read parser_expression` (§7.1). `plan` identifies the compiled
    /// [`ParserPlan`] in the process-wide arena; the runtime interpreter looks
    /// it up.
    Read {
        plan: praxis_input_parser::PlanId,
        ty: Type,
        span: (u32, u32),
    },
    /// `parse(text, parser_expression)` (§7.1). The `text` arg is lowered as
    /// an ordinary expression; `plan` identifies the parser plan.
    Parse {
        text: Box<TypedExpr>,
        plan: praxis_input_parser::PlanId,
        ty: Type,
        span: (u32, u32),
    },
    /// `Name { field: expr, … }` record literal (§4.5). `record_def_id`
    /// identifies the struct type (index into `TypeDb::record_defs`); `fields`
    /// are the lowered initializers in declaration order, each paired with its
    /// field index.
    RecordLit {
        record_def_id: praxis_types::RecordDefId,
        fields: Vec<(u32, TypedExpr)>,
        ty: Type,
        span: (u32, u32),
    },
    /// `receiver.0` — a tuple element, selected by position (§4.4).
    ///
    /// Its own variant rather than a `FieldGet` with a positional index: a record
    /// field and a tuple element lower to two different runtime symbols
    /// (`praxis_record_field` and `praxis_tuple_get`), so MIR must be told which
    /// one this is rather than re-deriving it from the receiver's type. Adding the
    /// variant is what sends the compiler to every exhaustive walk that has to
    /// know about it — the same reason `TypedExpr::FnValue` is a variant and not a
    /// flag on `Path` (ADR-061).
    TupleIndex {
        receiver: Box<TypedExpr>,
        index: u32,
        ty: Type,
        span: (u32, u32),
    },
    /// `receiver.field` — a record field access (§4.5). `field_idx` is the
    /// field's index in the record's `RecordDef`.
    FieldGet {
        receiver: Box<TypedExpr>,
        field_idx: u32,
        ty: Type,
        span: (u32, u32),
    },
    /// An enum variant construction (§4.6): `Number(5)` or bare `Empty`.
    /// `enum_def_id` identifies the enum, `variant_idx` the variant, and `args`
    /// are the payload values (empty for a payload-less variant).
    EnumVariant {
        enum_def_id: praxis_types::EnumDefId,
        variant_idx: u32,
        args: Vec<TypedExpr>,
        ty: Type,
        span: (u32, u32),
    },
    /// `match scrutinee { pattern => body, … }` (§4.6).
    Match {
        scrutinee: Box<TypedExpr>,
        arms: Vec<TypedMatchArm>,
        ty: Type,
        span: (u32, u32),
    },
    /// `|params| body` closure (§4.10). `fn_name` is a synthesized unique
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

/// A recursive pattern. Models the full pattern grammar: wildcard, literal,
/// variable bind, enum variant with nested sub-patterns, tuple and record
/// (§4.6). The exhaustiveness checker and the MIR decision-tree lowering both
/// recurse over this.
#[derive(Clone, Debug)]
pub enum TypedPattern {
    /// `_` — matches anything, binds nothing.
    Wildcard,
    /// A literal scalar value to test against (§4.6). The MIR emits an equality
    /// compare against the scrutinee for these.
    Lit { value: Lit, ty: Type },
    /// `x` — binds the whole scrutinee to `symbol` (always matches).
    ///
    /// `name` and `span` are the same pair [`TypedStmt::Var`] carries, and for
    /// the same reason: ADR-125 says a name a pattern introduces is a binding
    /// in exactly the sense a `var` is, so the crash debugger has to be able to
    /// print it as `name: Type = value` and `p` has to be able to bind it
    /// (ADR-139). They therefore have to reach MIR, not stop at the AST.
    Bind {
        symbol: SymbolId,
        name: String,
        ty: Type,
        span: (u32, u32),
    },
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
    /// `(a, b)` — matches each element against its sub-pattern (§4.4). Always
    /// matches the shape: a tuple has one constructor, so the test is the
    /// elements' and never the tuple's.
    Tuple {
        subpatterns: Vec<TypedPattern>,
        ty: Type,
    },
    /// `P { x, y: p }` — matches each field against its sub-pattern (§4.5).
    /// Like [`EnumVariant`](Self::EnumVariant)'s payload, `subpatterns` is
    /// **positional** over the record's declared fields and padded to their
    /// count, so a field the pattern does not name is a `Wildcard` and MIR reads
    /// slot *i* for sub-pattern *i*.
    Record {
        record_def_id: praxis_types::RecordDefId,
        subpatterns: Vec<TypedPattern>,
        ty: Type,
    },
}

/// Why `pat` can fail to match, or `None` when it matches every value of its
/// type.
///
/// A binding position has no second arm, so only the shapes that always match
/// may appear in one: a wildcard, a name, and a composite whose components are
/// themselves irrefutable. A literal tests, and an enum variant tests — a
/// `for Some(x) in xs` would silently skip every `None`.
///
/// The one caller that turns a reason into `Y125` is
/// [`crate::pattern::check_binding_patterns`], which runs at the end of analysis
/// (ADR-133). Lowering asks this question nowhere: a second copy of the rule
/// here would only ever fire on a case analysis missed, and would word it again.
pub(crate) fn refutable_reason(pat: &TypedPattern) -> Option<&'static str> {
    match pat {
        TypedPattern::Wildcard | TypedPattern::Bind { .. } => None,
        TypedPattern::Lit { .. } => Some("a literal pattern"),
        TypedPattern::EnumVariant { .. } => Some("a variant pattern"),
        TypedPattern::Tuple { subpatterns, .. } | TypedPattern::Record { subpatterns, .. } => {
            subpatterns.iter().find_map(refutable_reason)
        }
    }
}

impl TypedPattern {
    /// The sub-patterns this pattern matches against the components of the value
    /// it tests — a variant's payload, a tuple's elements, a record's fields —
    /// and an empty slice for a pattern with no components.
    ///
    /// Written once because three walks want it — the usefulness matrix asks it
    /// twice and MIR's decision tree once — and naming the composite variants by
    /// hand in each is how a newly added one silently becomes a catch-all in all
    /// three.
    #[must_use]
    pub fn sub_patterns(&self) -> &[TypedPattern] {
        match self {
            TypedPattern::Wildcard | TypedPattern::Lit { .. } | TypedPattern::Bind { .. } => &[],
            TypedPattern::EnumVariant { subpatterns, .. }
            | TypedPattern::Tuple { subpatterns, .. }
            | TypedPattern::Record { subpatterns, .. } => subpatterns,
        }
    }
}

/// Every variant's children, written **once**.
///
/// Each row is `Variant { field, field: shape, … }` — that variant's child
/// fields — and the macro expands the rows into the four accessors below. Adding
/// a variant is a compile error here (the match is exhaustive) rather than a
/// silent omission in a walk somewhere else — the failure mode of hand-writing a
/// ~29-arm walk per consumer, where a forgotten field (`Call.callee_expr`, say)
/// leaves that consumer quietly skipping a whole subtree.
///
/// The `shape` suffix says how to reach the children: a bare field is one
/// expression, `opt` is an `Option`, `each` is a sequence, `field_each`/`arm_each`
/// are the two sequences whose elements are not bare expressions, and
/// `block`/`block_opt` are sub-*blocks* rather than sub-expressions.
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
    FnValue {},
    Read {},
    Continue {},
    Bin { lhs, rhs },
    Range { start, end },
    Unary { operand },
    Paren { inner: opt },
    If { cond, then_block: block, else_block: block_opt },
    While { cond, body: block },
    For { iter, body: block },
    Loop { body: block },
    Break { value: opt },
    Return { value: opt },
    Call { args: each, callee_expr: opt },
    MethodCall { receiver, args: each },
    Tuple { elements: each },
    ListLit { elements: each },
    Parse { text },
    RecordLit { fields: field_each },
    Interp { parts: field_each },
    FieldGet { receiver },
    TupleIndex { receiver },
    EnumVariant { args: each },
    Match { scrutinee, arms: arm_each },
    Closure { body: block },
}

/// One arm of a lowered `match` expression (§4.6). The pattern is recursive
/// (see [`TypedPattern`]); the MIR lowering emits a decision tree over it.
#[derive(Clone, Debug)]
pub struct TypedMatchArm {
    /// The arm's pattern (recursive: may nest sub-patterns in variant payloads).
    pub pattern: TypedPattern,
    /// The arm body expression.
    pub body: TypedExpr,
}

/// A literal value. `Int`/`Bool`/`Unit` lower directly; `Text` materializes via
/// the runtime text descriptor; `Float` is §4.12's, and `Char` is both the input
/// parser's `char`/`grid(char)` element and the `'#'` literal's.
#[derive(Clone, Debug)]
pub enum Lit {
    Int(i64),
    /// An IEEE-754 binary64 value (the payload of a `Float` object, §4.12).
    Float(f64),
    Text(String),
    Bool(bool),
    /// A Unicode scalar value (the payload of a `Char` object).
    ///
    /// Built from a `'#'` character literal, in an expression and in a pattern
    /// alike (ADR-141), and from the input parser's `char`/`grid(char)`
    /// elements.
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
    // Logical (Bool -> Bool -> Bool). Both **short-circuit**, so MIR lowers each
    // to a branch rather than to an operation on two evaluated operands — which
    // is why they are one variant each and not a `BoolOp` with a flag.
    LogicalOr,
    LogicalAnd,
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

/// The name of the synthetic function holding a file's top-level statements
/// (ADR-067).
///
/// **Not an identifier**, and deliberately: the parser cannot produce this name,
/// so no program can declare a second function with it and no program can call
/// it. That is ADR-064's rule for the subscript rows, applied to the one other
/// name the compiler mints into the same namespace.
pub const ENTRY_NAME: &str = "<entry>";

/// The entry point of a compiled module, given the function names it defines.
///
/// A file's top-level statements are its program, so [`ENTRY_NAME`] wins when it
/// exists. A file with no top-level statements has none, and falls back to a
/// declared `fn main` — which is the convention every corpus program and every
/// end-to-end test is written in, and which the design doc never mentions.
///
/// Both hosts that execute a module (the CLI's `run` and the debugger's reload)
/// ask this, so the rule is in one place rather than two.
#[must_use]
pub fn entry_point<'n>(defines: impl Fn(&str) -> bool) -> Option<&'n str> {
    if defines(ENTRY_NAME) {
        Some(ENTRY_NAME)
    } else if defines("main") {
        Some("main")
    } else {
        None
    }
}

/// Whether a top-level node is a *statement* — something the entry point runs —
/// rather than a declaration.
///
/// The three declaration kinds are the exceptions and they are named positively:
/// a `fn` is lowered as its own item, and `struct`/`enum` are type-only and
/// produce no runtime item at all. Anything else at the top level is a
/// `var`/assignment/expression, which is a statement.
fn is_top_level_stmt(node: &SyntaxNode) -> bool {
    FnItem::cast(node.clone()).is_none()
        && StructItem::cast(node.clone()).is_none()
        && EnumItem::cast(node.clone()).is_none()
}

/// Lower a fully analyzed file into a typed tree.
///
/// `analysis` must be the result of [`analyze`](crate::analyze) on `root`; pass
/// the same `file` id so diagnostics carry correct spans. Never panics on a
/// program's account — a construct it cannot lower becomes a `Y0xx`/`Y1xx`
/// diagnostic in the returned module.
///
/// Takes `analysis` mutably because the [`TypeDb`] is still written to: the
/// cached scalar/unit handles, the entry point's `Func` type, `deep_resolve`'s
/// rewrites, and the parser-expression plans all mint or intern. No prior
/// result of the analysis changes — only the arena grows.
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
        // ADR-098's index is the language server's, not lowering's: lowering
        // runs `analyze_parser_expr`, which builds and registers a plan from the
        // rowan tree, and never reads a retained AST.
        parser_exprs: _,
        diagnostics: _,
    } = analysis;
    // Cache the scalar/unit handles once (these methods need &mut db).
    let int = db.int();
    let float = db.float();
    let bool_ = db.bool();
    let text = db.text();
    let unit = db.unit();
    // A file's top-level statements are its program. They need a function to
    // live in, and that function needs a symbol — minted here, before the lowerer
    // borrows the name table, and only when there is something to put in it. A
    // `Fn` symbol with no `decl` span: it is a real declaration, written by the
    // compiler rather than by the file.
    let entry_symbol = root.stmts().any(|node| is_top_level_stmt(&node)).then(|| {
        names.insert(crate::Symbol {
            id: crate::SymbolId(0), // overwritten by `insert`
            name: ENTRY_NAME.to_string(),
            kind: crate::SymbolKind::Fn,
            decl: None,
            reassigned: false,
            scheme: None,
        })
    });
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
    // The top-level statements, in source order, for the entry point. They are
    // interleaved with the `fn` items in the file and separated here: a `fn`
    // inside a `fn` is `N005`, so the entry point cannot simply be "the file",
    // and the declarations have to stay where they are.
    let mut entry_stmts = Vec::new();
    for node in root.stmts() {
        if let Some(fn_item) = FnItem::cast(node.clone()) {
            if let Some(tfn) = l.lower_fn(&fn_item) {
                items.push(TypedItem::Fn(tfn));
            }
        }
        // Struct declarations (§4.5) are type-only: they register a record
        // type during inference but produce no runtime item. Skip them here.
        if StructItem::cast(node.clone()).is_some() {
            // No codegen for the declaration itself.
        }
        // Enum declarations (§4.6) are likewise type-only.
        if EnumItem::cast(node.clone()).is_some() {
            // No codegen for the declaration itself.
        }
        // A top-level `var`/`expr`/`assign` **executes**: it goes into the entry
        // point, in the order it is written.
        if is_top_level_stmt(&node) {
            if let Some(stmt) = l.lower_stmt(&node) {
                entry_stmts.push(stmt);
            }
        }
    }
    if let Some(symbol) = entry_symbol {
        let span = {
            let r = root.syntax().text_range();
            (u32::from(r.start()), u32::from(r.end()))
        };
        let fn_type = l.db.func(Vec::new(), unit);
        items.push(TypedItem::Fn(TypedFn {
            symbol,
            name: ENTRY_NAME.to_string(),
            params: Vec::new(),
            return_type: unit,
            // A file has no value: every top-level statement runs for effect and
            // the tail is Unit. `out(overlaps(segments, false))` is a statement
            // here, not a result — which is why nothing is printed twice.
            //
            // The tail is **spanless** for `lower_block`'s reason: the file's own
            // range would give the entry point's return temp a span covering the
            // whole program, and the debugger renders a temp's span as its
            // `@ "expr"` provenance.
            body: TypedBlock {
                stmts: entry_stmts,
                tail: TypedExpr::Lit {
                    value: Lit::Unit,
                    ty: unit,
                    span: (0, 0),
                },
                ty: unit,
            },
            fn_type,
            span,
        }));
    }
    // Every binding some assignment writes. Read off the symbol table rather
    // than walked for, because name resolution already answered it at the one
    // point where the answer is decidable — see `Symbol::reassigned`.
    let reassigned_vars = l
        .names
        .all()
        .iter()
        .filter(|s| s.reassigned)
        .map(|s| s.id)
        .collect();
    TypedModule {
        items,
        diagnostics: l.diagnostics,
        // Escape analysis: every reassigned binding captured by cell, recorded
        // as each closure was lowered rather than re-derived by a walk
        // afterwards. These are boxed into a `VarCell` at their binding site so
        // the closure shares the cell.
        escaping_vars: l.escaping_vars,
        reassigned_vars,
    }
}

// The escape-analysis set is accumulated by the lowerer itself, in
// `Lowerer::lower_closure`, where every closure's capture list is already in
// hand — see `Lowerer::escaping_vars`. `CaptureKind::ByCell` and membership in
// `escaping_vars` are two representations of one fact, and only one of them is
// computed: a separate walk over the typed tree would silently under-approximate
// wherever it forgot an expression position (an immediately invoked closure,
// `(|n| { count = count + n })(1)`, hides one in `Call.callee_expr`).

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
    /// Each call site's monomorphization witness (§13.6), keyed by the callee
    /// name token's range. Read in `lower_call` to attach concrete arg types to
    /// each `TypedExpr::Call`.
    call_sites: &'a HashMap<TextRange, crate::CallSite>,
    /// Every inferred expression's type, keyed by its node. **This is where a
    /// lowered node's type comes from.** Lowering derives none of its own: a
    /// second instantiation of a scheme, or a fresh variable for a node it
    /// cannot place, agrees with whatever the next use wants and surfaces as a
    /// missing descriptor passes later.
    expr_types: &'a HashMap<crate::NodeKey, Type>,
    /// Each method call's resolved catalog entry and inferred result, keyed by
    /// the method-name token's range. Lowering reads the entry rather than
    /// repeating the catalog lookup against a receiver type of its own.
    method_refs: &'a HashMap<TextRange, crate::MethodRef>,
    /// Memo for [`Lowerer::deep`]: a recorded type, fully resolved to its
    /// leaves. `follow` only resolves the top level, so a `Vec[?T]` whose `?T` a
    /// later `push` pinned would reach codegen with no element descriptor.
    resolved: HashMap<Type, Type>,
    diagnostics: Vec<Diagnostic>,
    /// The built-in method catalog (§16.2). A resolved method call reads its row
    /// from `method_refs`; this is here for the one row lowering has to look up
    /// itself — the subscript *read* a compound `m[k] += v` needs, which no
    /// source token names. Immutable; built once.
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
    /// Every binding some closure captures **by cell**. Recorded in
    /// `lower_closure`, where the capture list is in hand, so the set cannot
    /// disagree with the `CaptureKind::ByCell` decisions that produce it. The MIR
    /// builder boxes each of these into a `VarCell` at its binding site; a
    /// binding missing here is one whose mutation a closure would write to a
    /// copy.
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
        FileSpan::new(self.file, range_to_span(range))
    }

    /// The byte span `[start, end)` of a rowan syntax node, as a `(u32, u32)`
    /// pair threaded onto the typed tree for debugger provenance (per-temp
    /// `@ "expr"` rendering) and diagnostics. Used at every `TypedExpr`/
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

    // --- reading what inference decided --------------------------------------

    /// The type inference recorded for `node`.
    ///
    /// A miss is `Y099`, never a fresh variable: inference visits every
    /// expression through one recording entry point, so a node lowering reaches
    /// and inference did not see is a compiler bug. A fresh variable would hide
    /// it — it agrees with whatever the next use wants, and the mistake surfaces
    /// as a missing descriptor three passes later.
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
        // arrived at, binders and all — and never a fresh *instantiation* of the
        // scheme, whose variables nothing else in this tree mentions. The params
        // below read their own monotypes and the body reads the inferred ones,
        // so an instantiation here would give a generic fn a `fn_type` that
        // disagrees with its own parameters, and `mono::specialize` substitutes
        // against exactly those variables.
        let scheme = self.names.get(symbol).and_then(|s| s.scheme.clone());
        let fn_type = match &scheme {
            Some(s) => {
                let body = s.body();
                self.deep(body)
            }
            None => {
                // No scheme inferred (the fn errored in analysis); skip rather
                // than cascade.
                return None;
            }
        };

        // Lowering does not track scopes at all (the analysis already did):
        // every name reference is resolved by its range through `self.refs`,
        // which is scope-independent and unambiguous.
        let _scope = self.scopes.root();

        let params = self.lower_params(item.param_list().as_ref());
        let return_type = self.fn_result(&fn_type);

        let body = item
            .body()
            .and_then(|b| self.lower_block(&b))
            .unwrap_or_else(|| self.unit_block(self.node_span(item.syntax())));

        Some(TypedFn {
            symbol,
            name,
            params,
            return_type,
            body,
            fn_type,
            // The whole `fn ... { ... }` declaration's byte span (§9.3).
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

    /// One lowered parameter — **or `None`**, and the `None` is load-bearing:
    /// `lower_params` and `lower_closure` both `filter_map` this, so answering
    /// `None` for a parameter that exists shortens the slot list and every
    /// parameter after it takes the wrong argument (`|_, b| b` would return the
    /// first). `None` is reserved for a parameter the *tree* does not have.
    fn lower_param(&mut self, p: &Param) -> Option<TypedParam> {
        // A **destructuring** closure parameter has no name of its own; its slot
        // symbol was declared at the pattern's range, and the pattern is taken
        // apart in the body by `destructure_pattern_params`.
        let Some(name_tok) = p.name() else {
            // A **wildcard** parameter: an anonymous slot, at the `_`'s own
            // range, holding an argument nothing in the body can name.
            if let Some(tok) = p.wildcard() {
                let symbol = self.resolve_decl_at(tok.text_range())?;
                let ty = self.symbol_type(symbol);
                return Some(TypedParam {
                    symbol,
                    name: Some("_".to_string()),
                    ty,
                });
            }
            let pat = p.pattern()?;
            let range = pat.syntax().text_range();
            let symbol = self.resolve_decl_at(range)?;
            let ty = self.symbol_type(symbol);
            // Nameless on purpose: the container is a slot the compiler needed,
            // and the names the programmer wrote are the components inside it.
            return Some(TypedParam {
                symbol,
                name: None,
                ty,
            });
        };
        let name = Some(name_tok.text().to_string());
        let range = name_tok.text_range();
        let symbol = self.resolve_decl_at(range)?;
        // A parameter is not an expression, so it has no entry in `expr_types`;
        // its type is the monotype inference attached to its symbol. That is a
        // read, not a re-derivation: `infer_param` writes `Scheme::monotype`,
        // which has no binders to instantiate.
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
            // A non-ExprStmt (var/assign) after a pending tail demotes the
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
        // A block whose last child is a statement has no value of its own, so
        // one is synthesized. It is **spanless**: no source text materializes
        // it, and the block's own range would be a lie that reaches the user. A
        // MIR local's span is what the crash debugger renders as `@ "expr"`
        // provenance and what `fault_span` reads to pick the line a frame
        // faulted on, and on both counts a temp claiming the whole block is
        // worse than a temp claiming nothing: the first prints the entire block
        // on one line, the second offers the widest possible span as a candidate
        // for the narrowest question there is.
        //
        // `error_expr`'s `(0, 0)` is the spelling for "synthetic, no source",
        // which is what this is. The debugger's own dead-scratch rule then
        // drops the temp entirely when it never received a value, which is the
        // right outcome for a slot no program text asked for.
        let tail = tail.unwrap_or_else(|| self.error_expr());
        let ty = expr_ty(&tail);
        Some(TypedBlock { stmts, tail, ty })
    }

    fn lower_stmt(&mut self, node: &SyntaxNode) -> Option<TypedStmt> {
        if let Some(var_) = VarStmt::cast(node.clone()) {
            return self.lower_var(&var_);
        }
        if let Some(assign) = AssignStmt::cast(node.clone()) {
            return self.lower_assign(&assign);
        }
        if let Some(assign) = praxis_ast::PlaceAssignStmt::cast(node.clone()) {
            return self.lower_place_assign(&assign);
        }
        if let Some(expr_stmt) = ExprStmt::cast(node.clone()) {
            if let Some(e) = expr_stmt.expr() {
                return Some(TypedStmt::Expr(self.lower_expr(&e)));
            }
        }
        None
    }

    /// A binding with no name — `var _ = f()` (ADR-049 D7) — still runs its
    /// initializer; it just keeps nothing. Lowering it to a statement
    /// expression is what makes the discard idiom a *discard* rather than a
    /// deletion: dropping the statement instead would silently remove the call.
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
        let op = Self::assign_op_of(stmt.op().map(|t| t.kind()));
        Some(TypedStmt::Assign {
            symbol,
            name,
            op,
            value,
            span: self.node_span(stmt.syntax()),
        })
    }

    /// The assignment operator a token spells. Shared by the name-target and
    /// place-target statements so the two cannot drift.
    fn assign_op_of(tok: Option<SyntaxKind>) -> AssignOp {
        match tok {
            Some(SyntaxKind::PLUS_EQ) => AssignOp::AddAssign,
            Some(SyntaxKind::MINUS_EQ) => AssignOp::SubAssign,
            Some(SyntaxKind::STAR_EQ) => AssignOp::MulAssign,
            Some(SyntaxKind::SLASH_EQ) => AssignOp::DivAssign,
            Some(SyntaxKind::PERCENT_EQ) => AssignOp::RemAssign,
            _ => AssignOp::Assign,
        }
    }

    /// `m[key] = v`, `counts[key] += 1`, `p.x = 5` — a store through a place
    /// (§4.5/§6.2).
    ///
    /// For a subscript, inference resolved the store row and recorded it at the
    /// target's node range (as it records a method at its name token), so this
    /// reads the entry rather than re-deriving one. A target that is not a place,
    /// or a receiver with no store row, was already reported there (`Y021`/`Y020`);
    /// lowering drops the statement, which is what every unresolvable statement
    /// here does.
    fn lower_place_assign(&mut self, stmt: &praxis_ast::PlaceAssignStmt) -> Option<TypedStmt> {
        // `min=`/`max=` write **without reading** — that is what §6.2's "an
        // absent entry accepts the first value" means, and the comparison is the
        // wrapper's. So they lower as a plain store whose row is a different one,
        // which inference already resolved (ADR-064).
        let place_op = stmt.op();
        let op = match place_op {
            praxis_ast::PlaceAssignOp::Set
            | praxis_ast::PlaceAssignOp::Min
            | praxis_ast::PlaceAssignOp::Max => AssignOp::Assign,
            praxis_ast::PlaceAssignOp::Add => AssignOp::AddAssign,
            praxis_ast::PlaceAssignOp::Sub => AssignOp::SubAssign,
            praxis_ast::PlaceAssignOp::Mul => AssignOp::MulAssign,
            praxis_ast::PlaceAssignOp::Div => AssignOp::DivAssign,
            praxis_ast::PlaceAssignOp::Rem => AssignOp::RemAssign,
        };
        let idx = match stmt.target()? {
            Expr::Index(idx) => idx,
            // A field store needs no catalog row at all: the slot comes from the
            // record definition, and both halves of a compound operator go
            // through the one `LoadField`/`StoreField` pair MIR emits.
            Expr::FieldGet(f) => {
                let (receiver, field_idx) = self.lower_field_place(&f)?;
                return Some(TypedStmt::FieldAssign {
                    receiver,
                    field_idx,
                    op,
                    value: self.lower_expr(&stmt.value()?),
                    span: self.node_span(stmt.syntax()),
                });
            }
            _ => return None,
        };
        let resolved = self.method_refs.get(&idx.syntax().text_range()).copied()?;
        let set = match &resolved.entry.lowering {
            praxis_stdlib::MethodLowering::RuntimeSymbol(sym) => *sym,
            // No subscript row is an intrinsic, and one would have no call for
            // MIR to emit here. Dropping the statement is the same answer an
            // unresolved store gets.
            //
            // No subscript row is a `ScalarPrimitive` either, and that one is
            // structural rather than incidental: a store answers `Unit`, and
            // `ScalarPrimitive` exists precisely for rows whose answer is a
            // scalar the caller wants unboxed (ADR-118 decision 6). A `[]=` row
            // written that way would be asking MIR for the scalar result of a
            // statement.
            praxis_stdlib::MethodLowering::Intrinsic(_)
            | praxis_stdlib::MethodLowering::ScalarPrimitive(_) => return None,
        };
        let receiver = idx.receiver().map(|r| self.lower_expr(&r))?;
        let indices: Vec<TypedExpr> = idx.indices().iter().map(|e| self.lower_expr(e)).collect();
        let value = self.lower_expr(&stmt.value()?);
        // A compound operator reads before it writes. The read symbol comes from
        // the *same* receiver type inference resolved the store against, never
        // from one lowering derives itself, so the pair always describes one
        // collection.
        let get = if op == AssignOp::Assign {
            None
        } else {
            let hits = crate::catalog::lookup(
                self.db,
                self.catalog,
                resolved.receiver,
                praxis_stdlib::catalog::INDEX_READ,
                indices.len(),
            );
            match hits.first().map(|e| &e.lowering) {
                Some(praxis_stdlib::MethodLowering::RuntimeSymbol(sym)) => Some(*sym),
                _ => return None,
            }
        };
        Some(TypedStmt::IndexAssign {
            receiver,
            indices,
            get,
            set,
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
            Expr::Interp(i) => self.lower_interp(i, span),
            Expr::Path(p) => self.lower_path(p),
            Expr::Bin(b) => self.lower_bin(b),
            Expr::Range(r) => self.lower_range(r),
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
            Expr::List(l) => self.lower_list(l),
            Expr::Read(r) => self.lower_read(r),
            Expr::Parse(p) => self.lower_parse(p),
            Expr::RecordLit(r) => self.lower_record_lit(r),
            Expr::FieldGet(f) => self.lower_field_get(f),
            Expr::TupleIndex(t) => self.lower_tuple_index(t),
            Expr::Index(i) => self.lower_index(i),
            Expr::Match(m) => self.lower_match(m),
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

    /// Lower a closure expression (§4.10). Runs capture analysis to find the free
    /// variables (each becomes one env slot), lowers the params and body, and
    /// produces a [`TypedExpr::Closure`] carrying a synthesized unique MIR
    /// function name. The closure's *type* (`fn_type`) is the inferred `Func`.
    ///
    /// The capture environment is a runtime concern (§4.10): the type system does
    /// not model it. A capture nothing reassigns copies the value into the env; a
    /// reassigned one shares a `VarCell` — the env holds the cell, and the
    /// binding site boxes the binding so writes are visible across frames.
    fn lower_closure(&mut self, c: &praxis_ast::ClosureExpr) -> TypedExpr {
        let span = self.node_span(c.syntax());
        let params: Vec<TypedParam> = c.params().filter_map(|p| self.lower_param(&p)).collect();
        // The body is an expression. If it is a block, lower it as one; otherwise
        // wrap the single expression as a block whose tail is that expression.
        let body = match c.body() {
            Some(praxis_ast::Expr::Block(b)) => self
                .lower_block(&b)
                .unwrap_or_else(|| self.unit_block(span)),
            Some(other) => {
                let tail = self.lower_expr(&other);
                let ty = expr_ty(&tail);
                TypedBlock {
                    stmts: Vec::new(),
                    tail,
                    ty,
                }
            }
            None => self.unit_block(span),
        };
        // A destructuring parameter takes its argument apart around the body.
        // Done here rather than in MIR because the language already has the
        // construct that does it: a one-arm `match` on the parameter's slot.
        let body = self.destructure_pattern_params(c, body, span);
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
                // assignment target is a write — `|n| { total = n }` finds
                // `total` at a range with no recorded type. The binding's own
                // scheme is the answer in that case; a polymorphic one is
                // skipped, because a capture of a generic binding needs the *use
                // site*'s instantiation, not the scheme.
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
                // A cell is what makes a *write* visible on both sides of the
                // capture, so only a binding something writes needs one — the
                // question is `reassigned`, not what syntax introduced the
                // binding. Every binding is assignable (ADR-125), so a parameter
                // or a `for` variable needs a cell on the same terms as a `var`,
                // and a `var` nothing reassigns would pay for a cell and two
                // runtime calls per access for nothing.
                let kind = if self.names.get(fv.symbol).is_some_and(|s| s.reassigned) {
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
        // A binding captured by cell escapes its frame, and this is where that is
        // decided — so this is where it is recorded. Deriving the set from a
        // later walk over the tree means any expression position the walk forgets
        // silently produces an unboxed capture.
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

    /// Wrap `body` in one `match` per **destructuring** closure parameter, so each
    /// pattern takes its own argument apart before the body runs.
    ///
    /// A closure still takes one value per parameter — MIR gives each a slot and
    /// binds it to the parameter's symbol — so a pattern parameter needs a slot for
    /// the argument *and* somewhere to take it apart. The somewhere is a construct
    /// the language already has: `match arg { pattern => body }`, one arm, over the
    /// slot's own symbol. Record and tuple patterns have exactly one constructor
    /// (ADR-069), so MIR emits the component reads with no tag to compare and no
    /// arm to fall through to — the same instructions a hand-written destructuring
    /// would need, and no new MIR.
    ///
    /// **A parameter has no second arm**, so a pattern that can fail is `Y125` —
    /// the same rule a `for` binding lives under: `|Some(n)| n` would have no
    /// answer for a `None` argument. That report is
    /// [`crate::pattern::check_binding_patterns`]'s (ADR-133) — lowering is the
    /// pass `praxis check` and the editor do not run — and here the pattern is
    /// only built.
    ///
    /// Parameters are wrapped in reverse so the first one's `match` ends up
    /// outermost, which is the order the arguments arrive in.
    fn destructure_pattern_params(
        &mut self,
        c: &praxis_ast::ClosureExpr,
        body: TypedBlock,
        span: (u32, u32),
    ) -> TypedBlock {
        let params: Vec<praxis_ast::Param> = c.params().collect();
        let mut block = body;
        for p in params.into_iter().rev() {
            // A named parameter binds its whole argument and needs nothing.
            if p.name().is_some() {
                continue;
            }
            let Some(pat) = p.pattern() else { continue };
            // `|_|` binds nothing (ADR-049 D7), so there is nothing to take apart.
            if matches!(pat.kind(), praxis_ast::PatternKind::Wildcard) {
                continue;
            }
            let range = pat.syntax().text_range();
            let Some(symbol) = self.resolve_decl_at(range) else {
                continue;
            };
            let param_ty = self.symbol_type(symbol);
            let pattern = self.lower_pattern(&pat, param_ty);
            let ty = block.ty;
            let scrutinee = TypedExpr::Path {
                symbol,
                ty: param_ty,
                span,
            };
            let arm = TypedMatchArm {
                pattern,
                body: TypedExpr::Block(Box::new(block)),
            };
            block = TypedBlock {
                stmts: Vec::new(),
                tail: TypedExpr::Match {
                    scrutinee: Box::new(scrutinee),
                    arms: vec![arm],
                    ty,
                    span,
                },
                ty,
            };
        }
        block
    }

    /// Mint a fresh, unique synthetic MIR function name for a closure.
    fn fresh_closure_name(&mut self) -> String {
        let n = self.closure_counter;
        self.closure_counter += 1;
        format!("__closure_{n}")
    }

    /// An interpolated literal (§8.1, ADR-147): the fragments' text, decoded, and
    /// the holes' expressions, lowered.
    ///
    /// The pairing walks the parts in source order and treats whatever fragment
    /// text has accumulated as "the text before the next hole". That is why a
    /// malformed run cannot lose anything: two fragments in a row (which the
    /// lexer cannot emit, but a tree mid-keystroke can hold) concatenate, and a
    /// missing closing fragment leaves `trailing` empty rather than dropping a
    /// hole.
    ///
    /// The **type** is read from inference like every other node's, not asserted
    /// to be `Text` here. Inference is the pass `praxis check` and the editor
    /// run, and a lowerer that decided a type its own would be a second answer
    /// (ADR-133).
    fn lower_interp(&mut self, i: &praxis_ast::InterpExpr, span: (u32, u32)) -> TypedExpr {
        let ty = self.node_ty(i.syntax());
        let mut parts: Vec<(String, TypedExpr)> = Vec::new();
        let mut pending = String::new();
        for part in i.parts() {
            match part {
                praxis_ast::InterpPart::Fragment(tok) => {
                    pending.push_str(&praxis_ast::interp_fragment_text(&tok));
                }
                praxis_ast::InterpPart::Hole(hole) => {
                    let lowered = self.lower_expr(&hole);
                    parts.push((std::mem::take(&mut pending), lowered));
                }
            }
        }
        TypedExpr::Interp {
            parts,
            trailing: pending,
            ty,
            span,
        }
    }

    fn lower_literal(&mut self, lit: &Literal) -> TypedExpr {
        let span = self.node_span(lit.syntax());
        // The *value* is read off the token; the *type* is what inference gave
        // this node. They agree for every well-formed literal — the point of
        // reading is the malformed ones, where inference has an answer and a
        // token-derived guess would not.
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
                // An `Int` is signed 64-bit (§4.3), so a literal outside that
                // range names a value the language cannot represent. Saturating
                // it is not an option: the saturated value is a perfectly good
                // `Int`, so the program would run with a number nobody wrote.
                //
                // **Inference reports it** (ADR-133), and `run` renders analysis
                // before it lowers, so this is not silence — it is the same
                // report, one pass earlier, where `check` and the editor can see
                // it too. Substituting `0` here keeps the typed tree buildable
                // for a caller that lowers anyway.
                let value = praxis_syntax::numeric::parse_int_literal(tok.text()).unwrap_or(0);
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
                // defensive fallback (substitute 0.0) rather than a panic — and
                // the separators have to come out first, because `3.141_592`
                // does not parse and 0.0 is not the number anybody wrote.
                let value = praxis_syntax::numeric::strip_digit_separators(text)
                    .parse::<f64>()
                    .unwrap_or(0.0);
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
            SyntaxKind::CharLit => {
                // A `'#'` character literal in expression position (ADR-141).
                //
                // A malformed literal substitutes U+0000 for the `IntLit` arm's
                // reason: the lexer reported `''`/`'ab'`/an unclosed quote and
                // `run` renders analysis before it lowers, so this is the same
                // report one pass earlier rather than silence, and the typed
                // tree stays buildable for a caller that lowers anyway.
                let value = praxis_syntax::literal::decode_char_literal(tok.text()).unwrap_or('\0');
                TypedExpr::Lit {
                    value: Lit::Char(value as u32),
                    ty,
                    span,
                }
            }
            // A template in value position is `Y023`, reported in inference, so
            // a well-formed program never reaches this arm — and an ill-formed
            // one is not lowered at all. It answers `Unit` rather than a
            // `Lit::Text` of the raw interior, which would be a plausible value:
            // `` `n = {int}` `` would *print itself* instead of failing. Lowering
            // after a reported error is a compiler bug, and this is the value
            // hardest to mistake for a program's.
            SyntaxKind::BacktickTemplate => TypedExpr::Lit {
                value: Lit::Unit,
                ty,
                span,
            },
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
        // The type is the one inference instantiated *at this reference*. Any
        // other answer — re-instantiating the symbol's scheme, or the enum def's
        // own type below — is a second instantiation whose variables nothing else
        // pinned.
        let ty = self.node_ty(p.syntax());
        // A zero-payload enum variant used as a bare path (`Empty`) — decided by
        // the symbol this name resolves to, not by its text.
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
            .unwrap_or(SymbolId::UNRESOLVED);
        // A top-level `fn` in value position is a function value, not a binding
        // reference (ADR-061). A `fn` reaches here only in value position —
        // `lower_call` resolves a named callee itself and never comes through
        // `lower_path` — so this is exactly the `var f = double` case.
        if self.symbol_kind(symbol) == Some(crate::SymbolKind::Fn) {
            return TypedExpr::FnValue {
                callee: symbol,
                callee_name: self
                    .names
                    .get(symbol)
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
                ty,
                span,
            };
        }
        TypedExpr::Path { symbol, ty, span }
    }

    /// `a..b` / `a..=b` (§4.11, ADR-059). Both bounds are ordinary `Int`
    /// expressions; the inclusiveness is read off the operator token.
    ///
    /// A bound the parser could not produce lowers as `Unit`, which cannot be an
    /// `Int` — the same defensive shape every other missing-operand path takes.
    /// Inference has already reported the malformed range.
    fn lower_range(&mut self, r: &praxis_ast::RangeExpr) -> TypedExpr {
        let span = self.node_span(r.syntax());
        let ty = self.node_ty(r.syntax());
        let (start, end) = r.bounds();
        let lower_bound = |this: &mut Self, bound: Option<praxis_ast::Expr>| match bound {
            Some(e) => Box::new(this.lower_expr(&e)),
            None => Box::new(TypedExpr::Lit {
                value: Lit::Unit,
                ty: this.unit,
                span,
            }),
        };
        let start = lower_bound(self, start);
        let end = lower_bound(self, end);
        TypedExpr::Range {
            start,
            end,
            inclusive: r.is_inclusive(),
            ty,
            span,
        }
    }

    fn lower_bin(&mut self, b: &BinExpr) -> TypedExpr {
        let span = self.node_span(b.syntax());
        let (lhs, rhs) = b.operands();
        let lhs = lhs.map(|e| Box::new(self.lower_expr(&e)));
        let rhs = rhs.map(|e| Box::new(self.lower_expr(&e)));
        // The *operator* is read off the token; the *type* is inference's. An
        // Int-or-Float decision made here would re-answer a question inference
        // has already settled, from a strictly narrower view of the operands.
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
            Some(SyntaxKind::AMP2) => BinOp::LogicalAnd,
            _ => BinOp::Add,
        };
        TypedExpr::Bin {
            op,
            lhs: lhs.unwrap_or_else(|| Box::new(self.error_expr())),
            rhs: rhs.unwrap_or_else(|| Box::new(self.error_expr())),
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
            operand: operand.unwrap_or_else(|| Box::new(self.error_expr())),
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
        // The `if`'s type is inference's join of its branches, read rather than
        // recomputed — and in particular never the then-block's alone, which
        // would make `if flag { panic("x") } else { 1 }` a `Never`.
        let ty = self.node_ty(i.syntax());
        TypedExpr::If {
            cond: cond.unwrap_or_else(|| Box::new(self.error_expr())),
            then_block: then_block.unwrap_or_else(|| Box::new(self.unit_block((0, 0)))),
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
            cond: cond.unwrap_or_else(|| Box::new(self.error_expr())),
            body: body.unwrap_or_else(|| Box::new(self.unit_block((0, 0)))),
            ty,
            span,
        }
    }

    /// `for binding in iter { body }` (§4.11).
    fn lower_for(&mut self, f: &ForExpr) -> TypedExpr {
        let span = self.node_span(f.syntax());
        let iter = f
            .iter()
            .map(|i| Box::new(self.lower_expr(&i)))
            .unwrap_or_else(|| Box::new(self.error_expr()));
        let body = f
            .body()
            .and_then(|b| self.lower_block(&b))
            .map(Box::new)
            .unwrap_or_else(|| Box::new(self.unit_block((0, 0))));
        // The item type is read from the iterator's inferred element type; the
        // inference pass records it on the binding **pattern**'s range. A `for`
        // binding is not an expression, so this is a `ref_types` read, not an
        // `expr_types` one.
        let item_ty = f
            .binding()
            .and_then(|p| self.ref_types.get(&p.syntax().text_range()).copied())
            .map(|t| self.deep(t))
            .unwrap_or(self.unit);
        // The binding is a pattern, and it has to match **every** item — a
        // `for` has no second arm to go to, so a pattern that can fail would
        // silently skip the steps it does not match — and that `Y125` is
        // `pattern::check_binding_patterns`'s, at the end of analysis (ADR-133),
        // because lowering is the pass `praxis check` and the editor do not run.
        // Lowering only builds the pattern.
        let binding = match f.binding() {
            Some(p) => self.lower_pattern(&p, item_ty),
            None => TypedPattern::Wildcard,
        };
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

    /// `loop { body }` (§4.11). The type is what its `break`s carry — `Never`
    /// when no `break` leaves the loop, `Unit` when they leave it with nothing.
    /// Inference computed that join; lowering reads it rather than keeping a
    /// second stack of loop frames to recompute it.
    fn lower_loop(&mut self, l: &LoopExpr) -> TypedExpr {
        let span = self.node_span(l.syntax());
        let body = l
            .body()
            .and_then(|b| self.lower_block(&b))
            .map(Box::new)
            .unwrap_or_else(|| Box::new(self.unit_block((0, 0))));
        let ty = self.node_ty(l.syntax());
        TypedExpr::Loop { body, ty, span }
    }

    /// `break [expr]` (§4.11). Diverges; the expression's type is `Never`.
    /// The *value* it carries reaches the loop through inference's join, which
    /// [`lower_loop`](Self::lower_loop) reads.
    fn lower_break(&mut self, b: &BreakExpr) -> TypedExpr {
        let span = self.node_span(b.syntax());
        let value = b.value().map(|v| Box::new(self.lower_expr(&v)));
        let ty = self.node_ty(b.syntax());
        TypedExpr::Break { value, ty, span }
    }

    /// `continue` (§4.11). Diverges; type `Never`.
    fn lower_continue(&mut self, c: &ContinueExpr) -> TypedExpr {
        let ty = self.node_ty(c.syntax());
        TypedExpr::Continue {
            ty,
            span: self.node_span(c.syntax()),
        }
    }

    /// `return [expr]` (§4.11). Diverges; type `Never`.
    fn lower_return(&mut self, r: &ReturnExpr) -> TypedExpr {
        let span = self.node_span(r.syntax());
        let value = r.value().map(|v| Box::new(self.lower_expr(&v)));
        let ty = self.node_ty(r.syntax());
        TypedExpr::Return { value, ty, span }
    }

    fn lower_call(&mut self, c: &CallExpr) -> TypedExpr {
        let span = self.node_span(c.syntax());
        let callee_tok = c.callee().and_then(|p| p.name());
        // Postfix call on an arbitrary expression (`expr(args)`, §4.10):
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
            .unwrap_or(SymbolId::UNRESOLVED);
        let callee_name = callee_tok
            .as_ref()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        let args: Vec<TypedExpr> = c
            .arg_list()
            .map(|a| self.lower_args(&a))
            .unwrap_or_default();
        // The call's result type is the one inference gave **this call site**,
        // never a fresh instantiation of the callee's scheme: the annotation in
        // `var v: Vec[Float] = Vec()` reaches the call site inference recorded,
        // and an instantiation here would answer an unbound variable instead.
        let ty = self.node_ty(c.syntax());
        // Enum variant construction — `Number(5)`. Decided by the symbol the
        // callee name resolves to, so a local shadowing a constructor is a call
        // of that local.
        if let Some((enum_def_id, variant_idx)) = self.enum_variant_of(callee) {
            return TypedExpr::EnumVariant {
                enum_def_id,
                variant_idx: variant_idx as u32,
                args,
                ty,
                span,
            };
        }
        // The concrete arg types at this call site (§13.6). Recorded by
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

    /// What kind of declaration `symbol` is, or `None` for the unresolved
    /// sentinel. The *kind* is what distinguishes a `fn` from a binding that
    /// holds a function value — a scheme cannot, since both are `Func`s, which is
    /// the same reason `SymbolKind::EnumVariant` is load-bearing in
    /// [`enum_variant_of`](Self::enum_variant_of).
    fn symbol_kind(&self, symbol: SymbolId) -> Option<crate::SymbolKind> {
        self.names.get(symbol).map(|s| s.kind)
    }

    /// The enum variant a **symbol** constructs: its def-id and variant index,
    /// or `None` when the symbol is not a variant constructor.
    ///
    /// Asked of the symbol resolution bound the name to, never of the name's
    /// *text* in the root scope: `enum E { A }` followed by `var A = 7` must
    /// lower the local `A` as a binding, and resolution is what answers which
    /// `A` a reference means.
    ///
    /// The kind check is load-bearing on its own: the scheme cannot distinguish
    /// a constructor from a binding that *holds* one, because `var A = Empty`
    /// has the enum type too.
    ///
    /// Unlike inference's counterpart this does **not** instantiate: lowering
    /// takes the use-site type from `expr_types` and needs only the variant's
    /// identity from here. It does not hand back the payload either — a generic
    /// def's payload is written in terms of its *parameters*, so reading it off
    /// the def would answer `T` rather than the element type.
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

    /// Lower `receiver.method(args)` (§16.2). Reads the catalog row inference
    /// resolved for this call site and records its runtime lowering symbol, so
    /// the MIR builder emits a direct call.
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

        // The catalog entry and the result type are what **inference** resolved
        // at this call site. Repeating the lookup here would re-derive the entry
        // from a receiver type of lowering's own and read the result off the
        // entry's *pattern* — a fresh `?T` for every `Var("T")` in it — so
        // `values.get(0)` on a `Vec[Float]` would lower as `?T` however firmly
        // inference had pinned it to `Float`.
        let resolved = m
            .method_name()
            .and_then(|t| self.method_refs.get(&t.text_range()).copied());
        let Some(resolved) = resolved else {
            // Inference could not resolve the method, and **inference reported
            // it** — ADR-093: a method call that cannot resolve is reported
            // there, either because the receiver is known and has no such row or
            // because no receiver in the catalog has that name at that arity.
            // Lowering reports nothing: it is the pass `praxis check` and the
            // editor never run, so a `Y110` owned here would be a silent `check`
            // followed by a failing `run`, and one diagnostic code has one
            // emitter.
            //
            // Two things can still land here, and neither wants a diagnostic.
            // A receiver **no call site pinned** — the body of a function
            // nothing calls — reaches lowering unresolved on purpose. It also
            // reaches MIR: `monomorphize` does not drop it, because ADR-057
            // decision 5 pins the receiver and a pinned receiver makes the
            // function a monotype, so `Scheme::is_polymorphic()` is false and
            // mono's filter keeps it. MIR recognizes the still-unbound receiver
            // and lowers the call to an unconditional `panic`, which is sound
            // because such a body is unreachable by construction (ADR-137
            // decision 3). And a chain that reaches MIR with a *concrete*
            // receiver and `lowering_symbol: None` is a compiler bug, which
            // surfaces as the MIR builder's ICE naming the method rather than as
            // a user-facing type error — a compiler bug should read as a
            // compiler bug report.
            //
            // The receiver and arguments are kept either way: they are
            // well-formed trees in their own right, and discarding them would
            // lose every closure and capture inside them.
            let ty = self.node_ty(m.syntax());
            return TypedExpr::MethodCall {
                receiver: Box::new(receiver),
                name,
                lowering_symbol: None,
                receiver_is_iterable: false,
                args,
                purity: praxis_stdlib::Purity::Impure,
                ty,
                span,
            };
        };
        let ty = self.deep(resolved.result);
        let lowering_symbol = match &resolved.entry.lowering {
            // A `ScalarPrimitive` row carries the same symbol and reaches MIR
            // the same way. What differs is what MIR *builds* from it — a
            // dedicated instruction rather than an `Inst::Call` — and MIR does
            // not ask this variant at all: `lower_method_call` asks the
            // *manifest* whether the row answers `AbiRet::RawI64`, so there is
            // no symbol list on that side to keep in step with this one
            // (ADR-118 decision 6). The typed tree therefore carries the
            // symbol and not the instruction, because nothing between here and
            // MIR has an opinion about the difference.
            //
            // What keeps the two in step is a pair of refusals rather than a
            // cross-check: `a_scalar_primitive_row_answers_the_scalar_channel_and_a_scalar_type`
            // refuses a `ScalarPrimitive` row whose wrapper still answers a
            // `GcRef`, and `build::lower_scalar_primitive`'s fallthrough is an
            // ICE for a `RawI64` wrapper no `Inst` produces.
            praxis_stdlib::MethodLowering::RuntimeSymbol(sym)
            | praxis_stdlib::MethodLowering::ScalarPrimitive(sym) => Some(*sym),
            // An intrinsic has no runtime symbol: the MIR builder lowers it
            // (the pipeline combinators) rather than emitting a call.
            praxis_stdlib::MethodLowering::Intrinsic(_) => None,
        };
        TypedExpr::MethodCall {
            receiver: Box::new(receiver),
            name,
            lowering_symbol,
            receiver_is_iterable: matches!(
                resolved.entry.receiver,
                praxis_stdlib::TypePattern::Iterable { .. }
            ),
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

    /// `[ a, b, … ]` — a `Vec` literal (§6.1). The same shape as a tuple's
    /// lowering, at a different node kind: the elements are lowered in source
    /// order and the type is the one inference recorded (ADR-054).
    fn lower_list(&mut self, l: &praxis_ast::ListExpr) -> TypedExpr {
        let span = self.node_span(l.syntax());
        let elements: Vec<TypedExpr> = l.elements().iter().map(|e| self.lower_expr(e)).collect();
        let ty = self.node_ty(l.syntax());
        TypedExpr::ListLit { elements, ty, span }
    }

    /// Lower a `read parser_expression` (§7.1). Analyzes the parser expr
    /// (validate + synthesize type + lower to plan) and produces a `TypedExpr`
    /// carrying the plan index; the node's *type* is inference's, as everywhere
    /// else here.
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

    /// Lower a `parse(text, parser_expression)` call (§7.1).
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
                    // Analysis failed and has already pushed a diagnostic. No
                    // `PlanId` means "none" — every value of it is a plan some
                    // program registered — so there is no plan to name here and
                    // an error expression is the honest lowering.
                    None => self.error_expr(),
                }
            }
            // No parser expression at all: the same "there is no plan" case as
            // above, and the same answer.
            None => self.error_expr(),
        }
    }

    /// A typed expression representing a lowering error (Unit-typed literal),
    /// and equally the placeholder a malformed subtree lowers to — a missing
    /// operand, condition, or iterator. One value for both, so no call site has
    /// to choose between two spellings of the same thing.
    ///
    /// No source span is available (this is a synthetic fallback), and `(0, 0)`
    /// is the spelling for that — see [`Lowerer::lower_block`] on why a
    /// synthesized expression claiming a real range is worse than one claiming
    /// nothing.
    fn error_expr(&self) -> TypedExpr {
        TypedExpr::Lit {
            value: Lit::Unit,
            ty: self.unit,
            span: (0, 0),
        }
    }

    /// The empty `Unit`-typed block that every construct with a block body
    /// falls back to when its body did not parse: a `fn`, a closure, and the
    /// bodies of `if`/`while`/`for`/`loop`.
    ///
    /// `span` is the synthesized tail literal's, not the block's: `(0, 0)` —
    /// [`Lowerer::error_expr`]'s "synthetic, no source" — where the fallback
    /// stands in for a subtree that produced no node at all, and the enclosing
    /// construct's own span where one is in hand.
    fn unit_block(&self, span: (u32, u32)) -> TypedBlock {
        TypedBlock {
            stmts: Vec::new(),
            tail: TypedExpr::Lit {
                value: Lit::Unit,
                ty: self.unit,
                span,
            },
            ty: self.unit,
        }
    }

    /// Lower a `Name { field: expr, … }` record literal (§4.5). Takes the record
    /// type from the type inference recorded for the node, pairs each
    /// initializer with its field index, and produces a `TypedExpr::RecordLit`.
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

    /// `p.0` — a tuple element (§4.4). Inference has already reported a receiver
    /// that is not a tuple, or an index past its arity, so this reads the index
    /// and the recorded type and does no checking of its own.
    fn lower_tuple_index(&mut self, t: &praxis_ast::TupleIndexExpr) -> TypedExpr {
        let span = self.node_span(t.syntax());
        let receiver = match t.receiver() {
            Some(r) => self.lower_expr(&r),
            None => return self.error_expr(),
        };
        let Some(index) = t.index().and_then(|i| u32::try_from(i).ok()) else {
            return self.error_expr();
        };
        let ty = self.node_ty(t.syntax());
        TypedExpr::TupleIndex {
            receiver: Box::new(receiver),
            index,
            ty,
            span,
        }
    }

    /// `m[key]`, `grid[x, y]` — a subscript read (§6.2).
    ///
    /// Lowers to a [`TypedExpr::MethodCall`] rather than a variant of its own,
    /// because that is what it *is* once the row is resolved: a runtime call with
    /// the receiver first and the indices after it, which is MethodCall's shape
    /// exactly. `name` carries the catalog spelling (`[]`), so a MIR dump reads as
    /// the source did.
    ///
    /// An unresolved subscript is reported in inference (`Y020`), so there is no
    /// report here — as with a method call's `Y110` (ADR-093), lowering emits
    /// nothing for either.
    fn lower_index(&mut self, i: &praxis_ast::IndexExpr) -> TypedExpr {
        let span = self.node_span(i.syntax());
        let receiver = match i.receiver() {
            Some(r) => self.lower_expr(&r),
            None => return self.error_expr(),
        };
        let args: Vec<TypedExpr> = i.indices().iter().map(|e| self.lower_expr(e)).collect();
        let resolved = self.method_refs.get(&i.syntax().text_range()).copied();
        let Some(resolved) = resolved else {
            return TypedExpr::MethodCall {
                receiver: Box::new(receiver),
                name: praxis_stdlib::catalog::INDEX_READ.to_string(),
                lowering_symbol: None,
                receiver_is_iterable: false,
                args,
                purity: praxis_stdlib::Purity::Impure,
                ty: self.node_ty(i.syntax()),
                span,
            };
        };
        let lowering_symbol = match &resolved.entry.lowering {
            // As `lower_method_call`: a subscript read carries its symbol and
            // MIR decides the instruction. No subscript row is a
            // `ScalarPrimitive` today — every one of them answers a `GcRef`
            // element — but the arm is written rather than declined, because
            // `v[i]` is the row ADR-118's open questions nominate next.
            praxis_stdlib::MethodLowering::RuntimeSymbol(sym)
            | praxis_stdlib::MethodLowering::ScalarPrimitive(sym) => Some(*sym),
            praxis_stdlib::MethodLowering::Intrinsic(_) => None,
        };
        TypedExpr::MethodCall {
            receiver: Box::new(receiver),
            name: praxis_stdlib::catalog::INDEX_READ.to_string(),
            lowering_symbol,
            receiver_is_iterable: matches!(
                resolved.entry.receiver,
                praxis_stdlib::TypePattern::Iterable { .. }
            ),
            args,
            purity: resolved.entry.purity,
            ty: self.deep(resolved.result),
            span,
        }
    }

    /// `r.field` — a record field read (§4.5).
    ///
    /// A receiver whose type is **still a variable here** is not an error. It
    /// means no call site ever said what the receiver is, which is the state
    /// §4.9's own fence is in:
    ///
    /// ```praxis
    /// fn manhattan(a, b) {
    ///     abs(a.x - b.x) + abs(a.y - b.y)
    /// }
    /// ```
    ///
    /// Nothing calls `manhattan`, so nothing pins `a`. Rejecting that read would
    /// make the program pass `praxis check` and then fail under `praxis run` with
    /// four `Y112`s — the `check`/`run` divergence this arm's silence exists to
    /// avoid — and it is the same tolerance an uncalled `fn f(a) { a + 1 }` has;
    /// a field read differs only in needing a record definition for an index.
    ///
    /// Silence here is affordable because it is not silence anywhere else:
    /// `Inferer::infer_field_get` requires `HasField` of *every* receiver, so a
    /// concrete one is rejected at the read and a deferred one is rejected when a
    /// call site resolves it — both at `praxis check`. What is left for lowering is
    /// only the receiver no pass can decide, and a function holding one has no
    /// instantiation to generate code for.
    fn lower_field_get(&mut self, f: &FieldExpr) -> TypedExpr {
        let span = self.node_span(f.syntax());
        let Some((receiver, field_idx)) = self.lower_field_place(f) else {
            return self.error_expr();
        };
        // The *index* comes from the record definition; the *type* from
        // inference, which already substituted the instance's arguments.
        let ty = self.node_ty(f.syntax());
        TypedExpr::FieldGet {
            receiver: Box::new(receiver),
            field_idx,
            ty,
            span,
        }
    }

    /// The receiver and the slot index a `receiver.field` names — the half a
    /// read and a store have in common (§4.5).
    ///
    /// One function rather than two derivations of "which slot is this", because
    /// a store that computed its own index could disagree with the read beside it
    /// in `p.x += 1` — and the record definition is the only thing that knows.
    /// `None` means there is nothing to lower; every reason for it is described
    /// in [`lower_field_get`](Self::lower_field_get)'s doc comment, and reported
    /// there or in inference.
    fn lower_field_place(&mut self, f: &FieldExpr) -> Option<(TypedExpr, u32)> {
        let receiver = self.lower_expr(&f.receiver()?);
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
            // A receiver nothing ever pinned: see `lower_field_get`'s doc
            // comment. Inference has already reported every receiver it could
            // decide.
            if self.db.var_id_of(resolved).is_some() {
                return None;
            }
            // A concrete type with no such field. Inference reports this too, and
            // `praxis run` stops before lowering when it does, so this is the
            // report for the callers that lower without checking first.
            if let Some(tok) = f.field_name() {
                self.diag(
                    tok.text_range(),
                    DiagCode::NoFieldOnType,
                    format!("no field `{}` on this type", tok.text()),
                );
            }
            return None;
        };
        Some((receiver, field_idx as u32))
    }

    /// Lower a `match scrutinee { pattern => body, … }` expression (§4.6).
    /// Each arm becomes a [`TypedMatchArm`] carrying a recursive
    /// [`TypedPattern`] and its body; the MIR builder lowers the arms to a
    /// decision tree.
    fn lower_match(&mut self, m: &praxis_ast::MatchExpr) -> TypedExpr {
        let span = self.node_span(m.syntax());
        let scrutinee = match m.scrutinee() {
            Some(s) => self.lower_expr(&s),
            None => return self.error_expr(),
        };
        let scrutinee_ty = expr_ty(&scrutinee);
        let mut arms = Vec::new();
        for arm in m.arms() {
            let pattern = match arm.pattern() {
                Some(pat) => self.lower_pattern(&pat, scrutinee_ty),
                None => TypedPattern::Wildcard,
            };
            let body = match arm.body() {
                Some(b) => self.lower_expr(&b),
                None => self.error_expr(),
            };
            arms.push(TypedMatchArm { pattern, body });
        }
        // **Coverage is not asked here** (ADR-130). `Y120`/`Y121` are decided at
        // the end of analysis — `exhaustive::check_matches`, run by `analyze` —
        // because a check that only runs where MIR is built is a check
        // `praxis check` and the editor never see. Lowering is reached only for
        // a program analysis accepted, so a match that reaches this point has
        // already been found exhaustive.
        //
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

    /// Lower a pattern into a recursive [`TypedPattern`].
    ///
    /// **The shape is [`crate::pattern::PatternBuilder`]'s** (ADR-130), not
    /// lowering's: the coverage check runs at the end of analysis, where there
    /// is no lowerer, and two builders would be free to disagree about which
    /// variant a bare name means — which is the one question `Y120` turns on.
    fn lower_pattern(&mut self, pat: &praxis_ast::Pattern, scrutinee_ty: Type) -> TypedPattern {
        crate::pattern::PatternBuilder {
            file: self.file,
            db: self.db,
            decls: self.decls,
            diagnostics: &mut self.diagnostics,
        }
        .build(pat, scrutinee_ty)
    }

    // --- helpers -----------------------------------------------------------

    /// Resolve the symbol *declared* at `range` (a `var`/`fn`/param name
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
/// debug-frame locals without re-matching the whole enum. `TypedExpr::Block` has
/// no span field of its own; it answers with its tail's.
pub fn expr_span(e: &TypedExpr) -> (u32, u32) {
    match e {
        TypedExpr::Lit { span, .. }
        | TypedExpr::Interp { span, .. }
        | TypedExpr::Path { span, .. }
        | TypedExpr::FnValue { span, .. }
        | TypedExpr::Bin { span, .. }
        | TypedExpr::Range { span, .. }
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
        | TypedExpr::ListLit { span, .. }
        | TypedExpr::Read { span, .. }
        | TypedExpr::Parse { span, .. }
        | TypedExpr::RecordLit { span, .. }
        | TypedExpr::FieldGet { span, .. }
        | TypedExpr::TupleIndex { span, .. }
        | TypedExpr::EnumVariant { span, .. }
        | TypedExpr::Match { span, .. }
        | TypedExpr::Closure { span, .. } => *span,
        TypedExpr::Block(b) => expr_span(&b.tail),
    }
}

/// A statement's immediate sub-expressions, in evaluation order.
///
/// Written once for the same reason [`TypedExpr::children`] is: three walks over
/// `TypedStmt` — MIR's closure collection, MIR's function-value collection, and
/// the debugger's purity check — would otherwise name the fields by hand, giving
/// a statement with more than one expression (`IndexAssign` has three) three
/// places to be forgotten in.
pub fn stmt_exprs(s: &TypedStmt) -> impl Iterator<Item = &TypedExpr> {
    let mut out: Vec<&TypedExpr> = Vec::new();
    match s {
        TypedStmt::Var { init, .. } => out.push(init),
        TypedStmt::Assign { value, .. } => out.push(value),
        TypedStmt::IndexAssign {
            receiver,
            indices,
            value,
            ..
        } => {
            out.push(receiver);
            out.extend(indices);
            out.push(value);
        }
        TypedStmt::FieldAssign {
            receiver, value, ..
        } => {
            out.push(receiver);
            out.push(value);
        }
        TypedStmt::Expr(e) => out.push(e),
    }
    out.into_iter()
}

/// [`stmt_exprs`], mutably — for the two statement rewrites monomorphization
/// runs (call retargeting and type specialization).
pub fn stmt_exprs_mut(s: &mut TypedStmt) -> impl Iterator<Item = &mut TypedExpr> {
    let mut out: Vec<&mut TypedExpr> = Vec::new();
    match s {
        TypedStmt::Var { init, .. } => out.push(init),
        TypedStmt::Assign { value, .. } => out.push(value),
        TypedStmt::IndexAssign {
            receiver,
            indices,
            value,
            ..
        } => {
            out.push(receiver);
            out.extend(indices);
            out.push(value);
        }
        TypedStmt::FieldAssign {
            receiver, value, ..
        } => {
            out.push(receiver);
            out.push(value);
        }
        TypedStmt::Expr(e) => out.push(e),
    }
    out.into_iter()
}

/// The source span `[start, end)` (byte offsets) carried by a typed statement.
/// `TypedStmt::Expr` carries the span on its inner expression.
pub fn stmt_span(s: &TypedStmt) -> (u32, u32) {
    match s {
        TypedStmt::Var { span, .. }
        | TypedStmt::Assign { span, .. }
        | TypedStmt::IndexAssign { span, .. }
        | TypedStmt::FieldAssign { span, .. } => *span,
        TypedStmt::Expr(e) => expr_span(e),
    }
}

/// The type carried by a typed expression. Public so the crash debugger and the
/// LSP can read an expression's inferred type without re-matching the whole
/// enum.
pub fn expr_ty(e: &TypedExpr) -> Type {
    match e {
        TypedExpr::Lit { ty, .. } => *ty,
        TypedExpr::Interp { ty, .. } => *ty,
        TypedExpr::Path { ty, .. } => *ty,
        TypedExpr::FnValue { ty, .. } => *ty,
        TypedExpr::Bin { ty, .. } => *ty,
        TypedExpr::Range { ty, .. } => *ty,
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
        TypedExpr::ListLit { ty, .. } => *ty,
        TypedExpr::Read { ty, .. } => *ty,
        TypedExpr::Parse { ty, .. } => *ty,
        TypedExpr::RecordLit { ty, .. } => *ty,
        TypedExpr::FieldGet { ty, .. } => *ty,
        TypedExpr::TupleIndex { ty, .. } => *ty,
        TypedExpr::EnumVariant { ty, .. } => *ty,
        TypedExpr::Match { ty, .. } => *ty,
        TypedExpr::Closure { ty, .. } => *ty,
    }
}

/// The public door onto [`pattern_to_type`] for the inference pass
/// (bidirectional method-call inference): instantiate a row's param/result
/// patterns before unification.
///
/// `names` carries the per-instantiation variable sharing — pass a *fresh* map
/// for each call site, or two unrelated uses of the same row would be forced to
/// the one type.
pub fn pattern_to_type_named(
    db: &mut TypeDb,
    p: &TypePattern,
    names: &mut HashMap<String, Type>,
) -> Type {
    pattern_to_type(db, p, names)
}

/// [`TypePattern::Iterable`] names ten types, so there is no one type to
/// instantiate it as (ADR-127 decision 1).
///
/// Reaching here is a catalog authoring mistake and not a program's, and it is
/// one `MethodCatalogBuilder::finish` already refuses: an `Iterable` may appear
/// only as a row's receiver, and a receiver is bound by `Inferer::bind_receiver`
/// — which unifies the row's *item* against `capability::iter_item` and never
/// instantiates the receiver at all.
fn iterable_is_not_a_type() -> ! {
    unreachable!(
        "internal compiler error: an `Iterable` pattern reached type \
         instantiation. It is a receiver shape, never a parameter or a result — \
         `MethodCatalogBuilder::finish` refuses a row that writes one elsewhere, \
         and `Inferer::bind_receiver` is the only thing that reads one."
    )
}

/// Convert a catalog [`TypePattern`] (the schema-level type of a method's
/// params and result) into a real inferred [`Type`] — the reverse of
/// [`crate::catalog::type_to_pattern`].
///
/// A single type variable is shared for each named `Var(name)` within one
/// instantiation, via `names`. This is what the bidirectional method-call
/// inference needs: a combinator signature like `fold`'s
/// `(Acc, (Acc, T) -> Acc) -> Acc` names the accumulator `Acc` in three places,
/// and those must be the *same* type variable so the accumulator type threads
/// from the init argument through the closure params to the result.
fn pattern_to_type(db: &mut TypeDb, p: &TypePattern, names: &mut HashMap<String, Type>) -> Type {
    match p {
        // `praxis_stdlib`'s pattern scalar *is* `praxis_types::ScalarType` (the
        // latter re-exports it), so there is nothing to map.
        TypePattern::Scalar(s) => db.scalar(*s),
        TypePattern::Unit => db.unit(),
        TypePattern::Var { name: n, .. } => {
            if let Some(&t) = names.get(*n) {
                t
            } else {
                let t = db.fresh_var();
                names.insert(n.to_string(), t);
                t
            }
        }
        TypePattern::Collection { ctor, args } => {
            let arg_tys: Vec<Type> = args.iter().map(|a| pattern_to_type(db, a, names)).collect();
            collection_from_pattern(db, *ctor, arg_tys)
        }
        TypePattern::Function { params, result } => {
            let ps: Vec<Type> = params
                .iter()
                .map(|p| pattern_to_type(db, p, names))
                .collect();
            let r = pattern_to_type(db, result, names);
            db.func(ps, r)
        }
        TypePattern::Tuple(els) => {
            let tys: Vec<Type> = els.iter().map(|e| pattern_to_type(db, e, names)).collect();
            tuple_or_degenerate(db, tys)
        }
        // The prelude's *one* `Option` def, instantiated at the inner pattern —
        // never a fresh def per row, or two rows' `Option`s would be unrelated
        // types. The inner pattern shares this instantiation's variables too:
        // `Map[K, V].get(K) -> Option[V]` names `V` twice and both must be the
        // one variable.
        TypePattern::Option(inner) => {
            let elem = pattern_to_type(db, inner, names);
            db.option_of(elem)
        }
        TypePattern::Iterable { .. } => iterable_is_not_a_type(),
    }
}

/// A collection type from a *catalog* [`TypePattern`], whose arity is
/// compiler-authored data rather than user input.
///
/// A row whose argument count disagrees with `ctor.arity()` is a bug in the
/// method catalog, not a program error, so it fails loudly here rather than
/// interning a type nothing can unify with (ADR-046).
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

/// A tuple type, honouring ADR-046's arity invariant: `()` is `Unit` and a lone
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

/// Strip surrounding quotes and unescape simple escapes from a `"…"` literal.
///
/// **This is not a decoder** — it is the local name for the workspace's one
/// decoder, `praxis_syntax::literal::unquote_text`. The rule lives in
/// `praxis-syntax` because the input parser's capture-body parser needs it too,
/// and a second copy would be free to unescape differently.
fn unquote_text(raw: &str) -> String {
    praxis_syntax::literal::unquote_text(raw)
}
