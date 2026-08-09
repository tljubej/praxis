//! The typed syntax-node wrappers over the parse tree (ADR-009).
//!
//! Each is a thin newtype over [`SyntaxNode`] with strongly-typed accessors. They
//! are intentionally minimal: only nodes walked by name resolution or type
//! inference appear here. The pattern is the rust-analyzer/rowan idiom — `cast`
//! by kind, `syntax` for the borrow, accessors that find the relevant child.
//!
//! The newtype and its [`AstNode`] impl are declared by `ast_node!`; the
//! accessors that follow each declaration are written by hand, since they are
//! the only part that differs between wrappers.

use praxis_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken};

use crate::{ast_node, child, children, name_token, token_matching, AstNode};

// ---------------------------------------------------------------------------
// Root + items
// ---------------------------------------------------------------------------

ast_node! {
    /// The root of a parsed file. Children are statements / `fn` items.
    SourceFile, SOURCE_FILE
}
impl SourceFile {
    /// The top-level statements and items, in source order. Items are returned
    /// generically as nodes; callers downcast via the wrapper [`cast`](crate::AstNode::cast).
    pub fn stmts(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax.children()
    }
}

ast_node! {
    /// A `var name = expr` binding — the language's one binding form (§4.2,
    /// ADR-125). A later `var` of the same name in the same scope shadows the
    /// earlier one and may carry an unrelated type.
    VarStmt, VAR_STMT
}
impl VarStmt {
    /// The bound name (a bare `Ident` token).
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The optional `: Type` annotation, if present.
    pub fn ty(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
    /// The initializer expression.
    pub fn init(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// A `name = expr` / `name += expr` reassignment.
    AssignStmt, ASSIGN_STMT
}
impl AssignStmt {
    /// The reassigned name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The assignment operator token (`=`, `+=`, …).
    pub fn op(&self) -> Option<SyntaxToken> {
        token_matching(&self.syntax, |k| {
            matches!(
                k,
                K::EQ | K::PLUS_EQ | K::MINUS_EQ | K::STAR_EQ | K::SLASH_EQ | K::PERCENT_EQ
            )
        })
    }
    /// The right-hand-side expression.
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// A top-level or nested `fn name(params) -> Ret { body }`.
    FnItem, FN_ITEM
}
impl FnItem {
    /// The function name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The `(params)` list, if present.
    pub fn param_list(&self) -> Option<ParamList> {
        child(&self.syntax)
    }
    /// The declared return type (after `->`), if any.
    pub fn return_type(&self) -> Option<TypeRef> {
        // The return type is the TypeRef that is not a ParamList child. Find the
        // TypeRef appearing after the THIN_ARROW token.
        self.syntax
            .children_with_tokens()
            .skip_while(|e| !matches!(e, rowan::NodeOrToken::Token(t) if t.kind() == K::THIN_ARROW))
            .skip(1) // skip the arrow itself
            .filter_map(|e| match e {
                rowan::NodeOrToken::Node(n) => TypeRef::cast(n),
                _ => None,
            })
            .next()
    }
    /// The function body block.
    pub fn body(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
}

ast_node! {
    /// A `struct Name { field: Type, … }` declaration (§4.5).
    StructItem, STRUCT_ITEM
}
impl StructItem {
    /// The struct name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The `{ field: Type, … }` field list.
    pub fn field_list(&self) -> Option<FieldList> {
        child(&self.syntax)
    }
}

ast_node! {
    /// An `enum Name { Variant, Variant(Type), … }` declaration (§4.6).
    EnumItem, ENUM_ITEM
}
impl EnumItem {
    /// The enum name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The variants in declaration order.
    pub fn variants(&self) -> impl Iterator<Item = EnumVariantNode> + '_ {
        self.syntax.children().filter_map(EnumVariantNode::cast)
    }
}

ast_node! {
    /// One variant of an enum declaration: `Name` or `Name(Type, …)` (§4.6).
    /// Named `EnumVariantNode` to avoid clashing with the type-system
    /// `EnumVariantDef`.
    EnumVariantNode, ENUM_VARIANT
}
impl EnumVariantNode {
    /// The variant name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The payload type(s), if the variant carries data. Returns `None` for a
    /// payload-less variant (`Empty`); `Some(vec)` for `Number(Int)` etc. Each
    /// payload type is a `TypeRef` child appearing after the name.
    pub fn payload_types(&self) -> Option<Vec<TypeRef>> {
        let types: Vec<TypeRef> = self.syntax.children().filter_map(TypeRef::cast).collect();
        if types.is_empty() {
            None
        } else {
            Some(types)
        }
    }
}

ast_node! {
    /// The `{ field: Type, … }` body of a struct, or the `{ field: expr, … }` body
    /// of a record literal. Reused for both declaration types and record-literal
    /// expressions.
    FieldList, FIELD_LIST
}
impl FieldList {
    /// The fields in declaration order.
    pub fn fields(&self) -> impl Iterator<Item = Field> + '_ {
        self.syntax.children().filter_map(Field::cast)
    }
}

ast_node! {
    /// A single `name: Type` field (in a struct) or `name: expr` / `name` (pun, in
    /// a record literal). §4.5.
    Field, FIELD
}
impl Field {
    /// The field name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The field's type (in a struct declaration), if present.
    pub fn ty(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
    /// The field's value expression (in a record literal), if present (`None`
    /// for a punned field `{ x }`).
    pub fn expr(&self) -> Option<Expr> {
        // The expression is the non-TypeRef, non-name child. For a pun there is
        // none; for `{ x: expr }` it follows the colon.
        self.syntax
            .children()
            .find(|c| TypeRef::cast(c.clone()).is_none())
            .and_then(Expr::cast_from_child)
    }
}

ast_node! {
    /// The `(...)` parameter list.
    ParamList, PARAM_LIST
}
impl ParamList {
    /// The parameters, in order.
    pub fn params(&self) -> impl Iterator<Item = Param> + '_ {
        children::<Param>(&self.syntax)
    }
}

ast_node! {
    /// A single `name: Type` parameter.
    Param, PARAM
}
impl Param {
    /// The parameter name, when the parameter *is* one.
    ///
    /// A `fn` parameter is a binder token and answers directly. A **closure**
    /// parameter is a pattern, so a bare-name one answers through that pattern.
    /// That is what lets closure lowering, resolution and inference go on asking
    /// a parameter for its name: `|x|` has a name here, and a destructuring one
    /// has none and is reached through [`Param::pattern`].
    pub fn name(&self) -> Option<SyntaxToken> {
        if let Some(tok) = name_token(&self.syntax) {
            return Some(tok);
        }
        let pat: Pattern = child(&self.syntax)?;
        match pat.kind() {
            PatternKind::Name(_) => pat.name_token(),
            _ => None,
        }
    }
    /// The parameter's pattern, for the position that has one (a closure). A
    /// `fn` parameter is a binder token and answers `None`.
    pub fn pattern(&self) -> Option<Pattern> {
        child(&self.syntax)
    }
    /// The `_` this parameter **is**, when it is one.
    ///
    /// A wildcard parameter binds no name — that is ADR-049 D7, and it is why
    /// [`Param::name`] answers `None` for it. It does not follow that there is no
    /// parameter: `|_, y| y` takes two arguments and returns the second. So the
    /// wildcard is reachable here, and a pass that needs one slot per parameter
    /// finds one for it rather than reading a slot list one short.
    ///
    /// Both spellings answer here, because the parser writes them differently: a
    /// `fn` parameter's `_` is a bare `UNDERSCORE` token (`expect_binder`), a
    /// closure's is a whole `Pattern` of kind [`PatternKind::Wildcard`]. A `_`
    /// *inside* a pattern — `|(a, _)|` — is a wildcard **component** and not this:
    /// it has no slot of its own, and the enclosing pattern owns the argument.
    pub fn wildcard(&self) -> Option<SyntaxToken> {
        fn underscore(node: &SyntaxNode) -> Option<SyntaxToken> {
            token_matching(node, |k| k == K::UNDERSCORE)
        }
        match self.pattern() {
            // `PatternKind::Wildcard` is also what a pattern node the parser gave
            // up on reports, so the token is the discriminator and not the kind.
            Some(pat) => underscore(pat.syntax()),
            None => underscore(&self.syntax),
        }
    }
    /// The declared parameter type.
    pub fn ty(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
}

ast_node! {
    /// A bare expression used as a statement (including trailing exprs in a block).
    ExprStmt, EXPR_STMT
}
impl ExprStmt {
    /// The contained expression.
    pub fn expr(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// The `:bp` marker a statement may end with (§9.8) — the syntax that puts a
    /// breakpoint in a program.
    ///
    /// It has no accessors, because it has nothing to say beyond *where it is*:
    /// the two tokens are fixed, and a consumer wants
    /// [`AstNode::span`](crate::AstNode::span) and nothing else.
    Breakpoint, BREAKPOINT
}

/// The `:bp` marker on `stmt`, if the statement carries one.
///
/// One function rather than an accessor per statement wrapper: the marker is
/// admitted at the end of *every* statement form ([`VarStmt`], [`AssignStmt`],
/// [`PlaceAssignStmt`], [`ExprStmt`]) and means the same thing on each, so four
/// identical bodies would be four places for the meaning to drift.
///
/// The search is over the node's **direct** children, which is what keeps a
/// marker on a nested statement from being read as this one's: a block statement
/// whose body ends in `out(x) :bp` has that marker inside the inner `EXPR_STMT`,
/// several levels down.
#[must_use]
pub fn breakpoint_marker(stmt: &SyntaxNode) -> Option<Breakpoint> {
    stmt.children().find_map(Breakpoint::cast)
}

// ---------------------------------------------------------------------------
// Type references
// ---------------------------------------------------------------------------

/// A type written in source: a scalar name, a tuple, or a function type.
///
/// The parser emits one of three node kinds for an annotation — [`K::TYPE_REF`]
/// for a name (with or without bracketed arguments), [`K::TUPLE_TYPE`] for
/// `(T, U)`, [`K::FN_TYPE`] for `(P) -> R` — and this wrapper accepts all
/// three, so that a written type is seen as one in every annotation position
/// (`var`, parameters, return types, struct fields, enum payloads) rather than
/// discarded for inference to reinvent.
///
/// Overriding `cast` is why this one is spelled out rather than declared by
/// `ast_node!` like every other wrapper.
#[derive(Clone, Debug)]
pub struct TypeRef {
    syntax: SyntaxNode,
}
impl AstNode for TypeRef {
    /// The kind a *name* annotation has. `cast` accepts the other two type node
    /// kinds as well — see [`SyntaxKind::is_type_node`](praxis_syntax::SyntaxKind::is_type_node).
    const KIND: K = K::TYPE_REF;
    /// Accepts every node kind an annotation can be, not just [`Self::KIND`].
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        syntax.kind().is_type_node().then(|| Self { syntax })
    }
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl TypeRef {
    /// For a scalar type, its name token (`Int`, `Text`, …).
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// Any expression, tagged by which node kind it is. Returned by block/item
/// accessors so callers can dispatch once without re-casting.
#[derive(Clone, Debug)]
pub enum Expr {
    Literal(Literal),
    /// `"a{x}b"` — an interpolated text literal (§8.1, ADR-147).
    ///
    /// Separate from [`Literal`](Self::Literal) because it is not a leaf: its
    /// holes are expression subtrees, and every walk that looks for names has to
    /// descend into them.
    Interp(InterpExpr),
    Path(PathExpr),
    Bin(BinExpr),
    /// `a..b` or `a..=b` — a range (§4.11, ADR-059).
    Range(RangeExpr),
    Unary(UnaryExpr),
    Paren(ParenExpr),
    Block(BlockExpr),
    If(IfExpr),
    While(WhileExpr),
    /// `for name in iter { body }` (§4.11).
    For(ForExpr),
    /// `loop { body }` (§4.11).
    Loop(LoopExpr),
    /// `break [expr]` (§4.11).
    Break(BreakExpr),
    /// `continue` (§4.11).
    Continue(ContinueExpr),
    /// `return [expr]` (§4.11).
    Return(ReturnExpr),
    Call(CallExpr),
    MethodCall(MethodCallExpr),
    Tuple(TupleExpr),
    /// `[ e1, e2, … ]` — a `Vec` literal (§6.1).
    List(ListExpr),
    /// `read parser_expression` (§7.1).
    Read(ReadExpr),
    /// `parse(text, parser_expression)` (§7.1).
    Parse(ParseExpr),
    /// `Name { field: expr, … }` record literal (§4.5).
    RecordLit(RecordLitExpr),
    /// `receiver.field` field access (§4.5).
    FieldGet(FieldExpr),
    /// `receiver.0` tuple element access (§4.4).
    TupleIndex(TupleIndexExpr),
    /// `receiver[index]` subscript (§4.7/§6.2/§6.4).
    Index(IndexExpr),
    /// `match scrutinee { pattern => expr, … }` (§4.6).
    Match(MatchExpr),
    /// `|params| expr` closure (§4.10).
    Closure(ClosureExpr),
    /// An unparseable expression the parser wrapped in a `PARSE_ERROR` node.
    Error(SyntaxNode),
}

impl Expr {
    /// The underlying syntax node for this expression, regardless of variant.
    /// Used by AST walks (e.g. closure capture analysis) that need the node's
    /// range and subtree.
    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Expr::Literal(e) => e.syntax(),
            Expr::Interp(e) => e.syntax(),
            Expr::Path(e) => e.syntax(),
            Expr::Bin(e) => e.syntax(),
            Expr::Range(e) => e.syntax(),
            Expr::Unary(e) => e.syntax(),
            Expr::Paren(e) => e.syntax(),
            Expr::Block(e) => e.syntax(),
            Expr::If(e) => e.syntax(),
            Expr::While(e) => e.syntax(),
            Expr::For(e) => e.syntax(),
            Expr::Loop(e) => e.syntax(),
            Expr::Break(e) => e.syntax(),
            Expr::Continue(e) => e.syntax(),
            Expr::Return(e) => e.syntax(),
            Expr::Call(e) => e.syntax(),
            Expr::MethodCall(e) => e.syntax(),
            Expr::Tuple(e) => e.syntax(),
            Expr::List(e) => e.syntax(),
            Expr::Read(e) => e.syntax(),
            Expr::Parse(e) => e.syntax(),
            Expr::RecordLit(e) => e.syntax(),
            Expr::FieldGet(e) => e.syntax(),
            Expr::TupleIndex(e) => e.syntax(),
            Expr::Index(e) => e.syntax(),
            Expr::Match(e) => e.syntax(),
            Expr::Closure(e) => e.syntax(),
            Expr::Error(n) => n,
        }
    }

    /// Try to cast a syntax node into an `Expr` of any known expression kind.
    /// Returns `None` for non-expression nodes. Public so passes that walk the
    /// AST subtree (e.g. closure capture analysis) can descend generically.
    pub fn cast(n: SyntaxNode) -> Option<Expr> {
        Self::cast_from_child(n)
    }

    /// Try to cast a child node into an `Expr` of any known expression kind.
    fn cast_from_child(n: SyntaxNode) -> Option<Expr> {
        Some(match n.kind() {
            K::LITERAL => Expr::Literal(Literal::from_syntax(n)),
            K::INTERP_EXPR => Expr::Interp(InterpExpr::from_syntax(n)),
            K::PATH_EXPR => Expr::Path(PathExpr::from_syntax(n)),
            K::BIN_EXPR => Expr::Bin(BinExpr::from_syntax(n)),
            K::RANGE_EXPR => Expr::Range(RangeExpr::from_syntax(n)),
            K::UNARY_EXPR => Expr::Unary(UnaryExpr::from_syntax(n)),
            K::PAREN_EXPR => Expr::Paren(ParenExpr::from_syntax(n)),
            K::BLOCK_EXPR => Expr::Block(BlockExpr::from_syntax(n)),
            K::IF_EXPR => Expr::If(IfExpr::from_syntax(n)),
            K::WHILE_EXPR => Expr::While(WhileExpr::from_syntax(n)),
            K::FOR_EXPR => Expr::For(ForExpr::from_syntax(n)),
            K::LOOP_EXPR => Expr::Loop(LoopExpr::from_syntax(n)),
            K::BREAK_EXPR => Expr::Break(BreakExpr::from_syntax(n)),
            K::CONTINUE_EXPR => Expr::Continue(ContinueExpr::from_syntax(n)),
            K::RETURN_EXPR => Expr::Return(ReturnExpr::from_syntax(n)),
            K::CALL_EXPR => Expr::Call(CallExpr::from_syntax(n)),
            K::METHOD_CALL_EXPR => Expr::MethodCall(MethodCallExpr::from_syntax(n)),
            K::TUPLE_EXPR => Expr::Tuple(TupleExpr::from_syntax(n)),
            K::LIST_EXPR => Expr::List(ListExpr::from_syntax(n)),
            K::READ_EXPR => Expr::Read(ReadExpr::from_syntax(n)),
            K::PARSE_EXPR => Expr::Parse(ParseExpr::from_syntax(n)),
            K::RECORD_LIT_EXPR => Expr::RecordLit(RecordLitExpr::from_syntax(n)),
            K::FIELD_EXPR => Expr::FieldGet(FieldExpr::from_syntax(n)),
            K::TUPLE_INDEX_EXPR => Expr::TupleIndex(TupleIndexExpr::from_syntax(n)),
            K::INDEX_EXPR => Expr::Index(IndexExpr::from_syntax(n)),
            K::MATCH_EXPR => Expr::Match(MatchExpr::from_syntax(n)),
            K::CLOSURE_EXPR => Expr::Closure(ClosureExpr::from_syntax(n)),
            K::PARSE_ERROR => Expr::Error(n),
            _ => return None,
        })
    }
}

ast_node! {
    /// `Name { field: expr, … }` — a record-literal expression (§4.5). The
    /// first child is the `PATH_EXPR` naming the struct type; the `FIELD_LIST` holds
    /// the field initializers (explicit `name: expr` or punned `name`).
    RecordLitExpr, RECORD_LIT_EXPR
}
impl RecordLitExpr {
    /// The struct name (a path) being constructed.
    pub fn name(&self) -> Option<PathExpr> {
        child(&self.syntax)
    }
    /// The `{ field: expr, … }` initializer list.
    pub fn field_list(&self) -> Option<FieldList> {
        child(&self.syntax)
    }
}

ast_node! {
    /// `receiver.0` — tuple element access (§4.4). The first child is the
    /// receiver expression; the index is the trailing `IntLit` token.
    ///
    /// Its own node rather than a `FieldExpr` holding an `IntLit`: an element is
    /// selected by **position** and the index must be a literal, where a field is
    /// selected by name. The two lower to two different runtime calls, and keeping
    /// them apart is what makes every exhaustive match downstream ask about both.
    TupleIndexExpr, TUPLE_INDEX_EXPR
}
impl TupleIndexExpr {
    /// The receiver expression (`p` in `p.0`).
    pub fn receiver(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The index token (`0` in `p.0`).
    pub fn index_token(&self) -> Option<SyntaxToken> {
        token_matching(&self.syntax, |k| k == K::IntLit)
    }
    /// The index as a number, or `None` when the literal does not fit a `usize`.
    ///
    /// A tuple's arity is small and the index is written by hand, so an
    /// out-of-`usize` literal is a mistake rather than a case to support — but it
    /// is *representable* in the source, so this answers `None` rather than
    /// panicking, and the caller reports it like any other bad index.
    pub fn index(&self) -> Option<usize> {
        self.index_token()?.text().parse().ok()
    }
}

ast_node! {
    /// `receiver[index]` — a subscript. The first child is the receiver
    /// expression; the `ARG_LIST` holds the indices.
    ///
    /// The index list is a list rather than one expression because §6.4's
    /// `grid[x, y]` takes two. Arity is part of what selects the operation — a
    /// one-index `Grid` subscript is as much a mistake as a two-index `Map` one — so
    /// it is carried rather than flattened.
    IndexExpr, INDEX_EXPR
}
impl IndexExpr {
    /// The receiver expression (`m` in `m[key]`).
    pub fn receiver(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The `[index, …]` list.
    pub fn index_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }
    /// The index expressions, in source order.
    pub fn indices(&self) -> Vec<Expr> {
        self.index_list()
            .map(|l| l.args().collect())
            .unwrap_or_default()
    }
}

ast_node! {
    /// `place = expr` / `place += expr` — a reassignment whose target is an
    /// expression rather than a name: `m[key] = v`, `counts[key] += 1`.
    ///
    /// Two expression children, in source order: the target and the value. A bare
    /// `name = expr` is an [`AssignStmt`] instead, whose target is a *token* — which
    /// is why this is a second node and not a widened first one.
    PlaceAssignStmt, PLACE_ASSIGN_STMT
}
impl PlaceAssignStmt {
    /// The assignment target — the expression left of the operator.
    pub fn target(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// What this statement does to the place it names.
    ///
    /// A [`PlaceAssignOp`] and not the operator *token*, because `min=`/`max=`
    /// has no token of its own — it is an `Ident` and an `=` under an
    /// `UPDATE_OP` node. A token accessor would answer `None` for those two and
    /// every caller would read them as a plain store, which is the difference
    /// between "keep the smaller value" and "overwrite it".
    pub fn op(&self) -> PlaceAssignOp {
        use rowan::NodeOrToken;
        for child in self.syntax.children_with_tokens() {
            match child {
                NodeOrToken::Token(t) => {
                    return match t.kind() {
                        K::EQ => PlaceAssignOp::Set,
                        K::PLUS_EQ => PlaceAssignOp::Add,
                        K::MINUS_EQ => PlaceAssignOp::Sub,
                        K::STAR_EQ => PlaceAssignOp::Mul,
                        K::SLASH_EQ => PlaceAssignOp::Div,
                        K::PERCENT_EQ => PlaceAssignOp::Rem,
                        _ => continue,
                    };
                }
                NodeOrToken::Node(n) if n.kind() == K::UPDATE_OP => {
                    let is_min = n.children_with_tokens().any(
                        |e| matches!(e, NodeOrToken::Token(t) if t.kind() == K::Ident && t.text() == "min"),
                    );
                    return if is_min {
                        PlaceAssignOp::Min
                    } else {
                        PlaceAssignOp::Max
                    };
                }
                NodeOrToken::Node(_) => {}
            }
        }
        // The node is only built when an operator was seen, so this is
        // unreachable for any tree the parser produces.
        PlaceAssignOp::Set
    }
    /// The value expression — the one right of the operator.
    pub fn value(&self) -> Option<Expr> {
        self.syntax
            .children()
            .filter_map(Expr::cast_from_child)
            .nth(1)
    }
}

/// What a `place op= value` statement does.
///
/// Every spelling of the operator, in one enum, so the two that are not a token
/// cannot be forgotten: `min=` and `max=` are an identifier and an `=` (§6.2),
/// decided contextually because `min` is a name a program may use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaceAssignOp {
    /// `m[k] = v` — replace.
    Set,
    /// `m[k] += v` and its four arithmetic siblings — read, compute, write.
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// `d[k] min= v` — keep the smaller, or accept the first value (§6.2).
    Min,
    /// `b[k] max= v` — keep the larger, or accept the first value (§6.2).
    Max,
}

impl PlaceAssignOp {
    /// Whether this operator reads the place before writing it — true for the
    /// five arithmetic compounds and **false** for `min=`/`max=`, whose whole
    /// point is that they do not: a subscript read of an absent key faults
    /// (§4.7), and §6.2 says an absent entry accepts the first value.
    #[must_use]
    pub fn reads_before_writing(self) -> bool {
        matches!(
            self,
            PlaceAssignOp::Add
                | PlaceAssignOp::Sub
                | PlaceAssignOp::Mul
                | PlaceAssignOp::Div
                | PlaceAssignOp::Rem
        )
    }
}

ast_node! {
    /// `receiver.field` — field access (§4.5). The first child is the receiver
    /// expression; the field name is the trailing `Ident` token.
    FieldExpr, FIELD_EXPR
}
impl FieldExpr {
    /// The receiver expression (`p` in `p.x`).
    pub fn receiver(&self) -> Option<Expr> {
        // The receiver is the first expression-kind child.
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The field name being accessed (the trailing `Ident` after the dot).
    pub fn field_name(&self) -> Option<SyntaxToken> {
        // Take the last Ident token child (the field name); the receiver is a
        // child *node*, so it won't match this token scan.
        self.syntax
            .children_with_tokens()
            .filter_map(|e| match e {
                rowan::NodeOrToken::Token(t) if t.kind() == K::Ident => Some(t),
                _ => None,
            })
            .last()
    }
}

ast_node! {
    /// `match scrutinee { pattern => expr, … }` (§4.6/§4.11).
    MatchExpr, MATCH_EXPR
}
impl MatchExpr {
    /// The scrutinee expression being matched.
    pub fn scrutinee(&self) -> Option<Expr> {
        // The scrutinee is the first expression-kind child (before the arms).
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The match arms in source order.
    pub fn arms(&self) -> impl Iterator<Item = MatchArm> + '_ {
        self.syntax.children().filter_map(MatchArm::cast)
    }
}

ast_node! {
    /// One `pattern => expr` arm of a match expression (§4.6).
    MatchArm, MATCH_ARM
}
impl MatchArm {
    /// The arm's pattern.
    pub fn pattern(&self) -> Option<Pattern> {
        child(&self.syntax)
    }
    /// The arm's body expression (after `=>`).
    pub fn body(&self) -> Option<Expr> {
        // The body is the expression child that is not the Pattern.
        self.syntax
            .children()
            .filter(|c| c.kind() != K::PATTERN)
            .find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// `|params| expr` — a closure expression (§4.10). The params are `PARAM`
    /// children (no `PARAM_LIST` wrapper, since closures use `|…|` not `(…)`). The
    /// body is the trailing expression child. Closures capture outer variables
    /// automatically; the capture analysis lives in HIR.
    ClosureExpr, CLOSURE_EXPR
}
impl ClosureExpr {
    /// The closure's parameters (bare `name` or `name: Type`), in order.
    pub fn params(&self) -> impl Iterator<Item = Param> + '_ {
        children::<Param>(&self.syntax)
    }
    /// The closure body expression (after the closing `|`).
    pub fn body(&self) -> Option<Expr> {
        // The body is the expression child that is not a PARAM.
        self.syntax
            .children()
            .filter(|c| c.kind() != K::PARAM)
            .find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// A pattern (§4.6): `_`, literal, variable bind, or enum variant
    /// (optionally with sub-patterns).
    Pattern, PATTERN
}
impl Pattern {
    /// The kind of this pattern, as a [`PatternKind`].
    ///
    /// Read off the node's **direct** children, which is what keeps the shapes
    /// apart: a tuple pattern's elements and a record pattern's field names are
    /// nested one node deeper, so `(_, x)` is not a wildcard, `(1, 2)` is not a
    /// literal, and `P { x }` names `P` and not `x`.
    ///
    /// A pattern is a *record* pattern because of its brace, not because it has
    /// fields (ADR-091 Decision 3). Deciding it from the fields instead would be
    /// two silent catch-alls at once: `P {}` has no `PATTERN_FIELD` child and
    /// would fall through to a **binding** named `P` matching every value, and a
    /// headless `{ a, b }` has no direct `Ident` either and would reach the final
    /// `Wildcard` below — both irrefutable arms.
    pub fn kind(&self) -> PatternKind {
        let syntax = &self.syntax;
        // Check for wildcard.
        if syntax
            .children_with_tokens()
            .any(|e| matches!(e, rowan::NodeOrToken::Token(t) if t.kind() == K::UNDERSCORE))
        {
            return PatternKind::Wildcard;
        }
        // Check for a literal. The set is `SyntaxKind::is_pattern_literal`, the
        // one the parser accepts here.
        if syntax
            .children_with_tokens()
            .any(|e| matches!(e, rowan::NodeOrToken::Token(t) if t.kind().is_pattern_literal()))
        {
            return PatternKind::Literal;
        }
        // Check for an Ident — record, variant or variable bind.
        let name_tok = name_token(syntax);
        // `Name { … }` or a headless `{ … }` — a record pattern (ADR-091).
        // Decided *before* the name, and from the brace: the head is optional,
        // and a head with empty braces is still a record pattern. The
        // `PATTERN_FIELD` half stays as a second witness, so a tree whose brace
        // the parser lost still reads as the record it was written as.
        let has_brace = syntax
            .children_with_tokens()
            .any(|e| matches!(e, rowan::NodeOrToken::Token(t) if t.kind() == K::L_BRACE));
        if has_brace || syntax.children().any(|c| c.kind() == K::PATTERN_FIELD) {
            return PatternKind::Record(name_tok.map(|t| t.text().to_string()));
        }
        if let Some(tok) = name_tok {
            // If followed by `(`, it's a variant with sub-patterns.
            let has_parens = syntax.children().any(|c| c.kind() == K::PATTERN);
            if has_parens {
                return PatternKind::Variant(tok.text().to_string());
            }
            return PatternKind::Name(tok.text().to_string());
        }
        // No name and no literal: `(a, b)` — a tuple pattern. The parenthesis is
        // what distinguishes it from a node the parser gave up on, which has
        // neither.
        if syntax
            .children_with_tokens()
            .any(|e| matches!(e, rowan::NodeOrToken::Token(t) if t.kind() == K::L_PAREN))
        {
            return PatternKind::Tuple;
        }
        PatternKind::Wildcard
    }
    /// Sub-patterns, in order: a variant pattern's payload (`Number(n, _)`) or a
    /// tuple pattern's elements (`(a, b)`). A record pattern's are reached
    /// through [`Pattern::fields`], since each carries a field name.
    pub fn sub_patterns(&self) -> impl Iterator<Item = Pattern> + '_ {
        self.syntax.children().filter_map(Pattern::cast)
    }
    /// The fields of a record pattern `P { x, y: p }`, in source order.
    pub fn fields(&self) -> impl Iterator<Item = PatternField> + '_ {
        children::<PatternField>(&self.syntax)
    }
    /// The name token, if this is a variant or variable-bind pattern.
    pub fn name_token(&self) -> Option<SyntaxToken> {
        crate::name_token(&self.syntax)
    }

    /// The literal token, if this is a literal pattern (`42`, `"hi"`, `'#'`,
    /// `true`, `false`). Used by the HIR lowerer to read the value the pattern
    /// tests against.
    ///
    /// The set is
    /// [`SyntaxKind::is_pattern_literal`](praxis_syntax::SyntaxKind::is_pattern_literal),
    /// so it cannot disagree with [`Pattern::kind`]'s: a token that made a
    /// pattern `Literal` there and nothing here would leave the lowerer with
    /// `None` and an irrefutable wildcard.
    pub fn literal_token(&self) -> Option<SyntaxToken> {
        token_matching(&self.syntax, K::is_pattern_literal)
    }
}

ast_node! {
    /// One `name` or `name: pattern` field of a record pattern (§4.5).
    PatternField, PATTERN_FIELD
}
impl PatternField {
    /// The field's name token.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The sub-pattern the field is matched against, or `None` when the field is
    /// punned (`P { x }`) — which binds the field to its own name.
    pub fn pattern(&self) -> Option<Pattern> {
        child(&self.syntax)
    }
}

/// What kind of pattern a [`Pattern`] is (§4.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternKind {
    /// `_` — matches anything.
    Wildcard,
    /// A literal: `42`, `"hi"`, `'#'`, `true`, `false`.
    Literal,
    /// A variable bind: `x` — matches anything and binds the value to `x`.
    Name(String),
    /// An enum variant: `Empty` or `Number(…)` — the string is the variant name.
    Variant(String),
    /// A tuple: `(a, b)` — matches by position (§4.4).
    Tuple,
    /// A record: `P { x, y: p }` or a headless `{ x, y: p }` — matches by field
    /// name (§4.5, ADR-091). The string is the record's name when the pattern
    /// names one; a headless pattern pins its record from the *scrutinee*, the
    /// way a tuple pattern does (ADR-069 Decision 4), which is what lets an
    /// anonymous `choice(...)` payload — a record with no name to write — be
    /// taken apart at all.
    Record(Option<String>),
}

ast_node! {
    /// A literal: `IntLit`, `FloatLit`, `TextLit`, `CharLit`, `true`/`false`, and
    /// either backtick template — the set is
    /// [`SyntaxKind::is_literal_token`](praxis_syntax::SyntaxKind::is_literal_token).
    Literal, LITERAL
}
impl Literal {
    /// The single literal token.
    ///
    /// The set is [`SyntaxKind::is_literal_token`](praxis_syntax::SyntaxKind::is_literal_token),
    /// which is the parser's own, so every `LITERAL` the parser builds answers
    /// here rather than reading as a node with no token.
    pub fn token(&self) -> Option<SyntaxToken> {
        use rowan::NodeOrToken;
        self.syntax
            .children_with_tokens()
            .filter_map(|e| match e {
                NodeOrToken::Token(t) => Some(t),
                NodeOrToken::Node(_) => None,
            })
            .find(|t| t.kind().is_literal_token())
    }
}

ast_node! {
    /// An interpolated text literal: `"a{x}b"` (§8.1, ADR-147).
    InterpExpr, INTERP_EXPR
}

/// One piece of an interpolated literal, in source order.
#[derive(Clone, Debug)]
pub enum InterpPart {
    /// Literal text between the delimiters, already **undecoded** — the caller
    /// gets the token so it can decode with the workspace's one decoder and
    /// report against the token's own range.
    Fragment(SyntaxToken),
    /// A hole's expression.
    Hole(Expr),
}

impl InterpExpr {
    /// The pieces of the literal, in source order: fragment, hole, fragment,
    /// hole, …, fragment.
    ///
    /// Read off `children_with_tokens` rather than reconstructed from a count,
    /// because the order is the thing that matters and a malformed run (an
    /// `InterpClose` the parser had to synthesize, say) still yields the pieces
    /// it does have. A consumer that needs the invariant "n holes, n+1
    /// fragments" must not assume it: the tree is lossless, and a file being
    /// typed in an editor is malformed most of the time.
    pub fn parts(&self) -> Vec<InterpPart> {
        use rowan::NodeOrToken;
        self.syntax
            .children_with_tokens()
            .filter_map(|e| match e {
                NodeOrToken::Token(t)
                    if matches!(t.kind(), K::InterpOpen | K::InterpMiddle | K::InterpClose) =>
                {
                    Some(InterpPart::Fragment(t))
                }
                NodeOrToken::Token(_) => None,
                NodeOrToken::Node(n) => Expr::cast(n).map(InterpPart::Hole),
            })
            .collect()
    }
}

/// Strip an interpolation fragment's two delimiters and decode its escapes.
///
/// One byte comes off each end whichever fragment this is — `"a{`, `}b{` and
/// `}c"` all carry a delimiter at each end (ADR-147) — and the body goes through
/// [`praxis_syntax::literal::decode_text_body`], the workspace's one decoder, so
/// `"a\tb"` and `"a\tb{x}"` cannot disagree about what `\t` is.
///
/// A token too short to have two delimiters answers the empty string. That is
/// unreachable from the lexer, which never emits a fragment under two bytes, and
/// it is written as a value rather than a panic because the caller is a lowerer
/// running over a tree an editor may have caught mid-keystroke.
#[must_use]
pub fn interp_fragment_text(token: &SyntaxToken) -> String {
    let raw = token.text();
    if raw.len() < 2 {
        return String::new();
    }
    praxis_syntax::literal::decode_text_body(&raw[1..raw.len() - 1])
}

ast_node! {
    /// A name used as a value, or a callee (a bare `Ident` token, possibly followed
    /// by a call — the call wrapper wraps the path).
    PathExpr, PATH_EXPR
}
impl PathExpr {
    /// The name being referenced.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
}

ast_node! {
    /// A binary operator expression `a op b`.
    BinExpr, BIN_EXPR
}
impl BinExpr {
    /// The operator token (`+`, `==`, …).
    pub fn op(&self) -> Option<SyntaxToken> {
        token_matching(&self.syntax, |k| {
            matches!(
                k,
                K::PLUS
                    | K::MINUS
                    | K::STAR
                    | K::SLASH
                    | K::PERCENT
                    | K::EQ2
                    | K::NEQ
                    | K::LT
                    | K::GT
                    | K::LTEQ
                    | K::GTEQ
                    | K::PIPE2
                    | K::AMP2
            )
        })
    }
    /// The left and right operands (children that are expressions).
    pub fn operands(&self) -> (Option<Expr>, Option<Expr>) {
        let mut ops: Vec<_> = self
            .syntax
            .children()
            .filter_map(Expr::cast_from_child)
            .collect();
        // The operands are the two non-operator children, in order.
        let rhs = ops.pop();
        let lhs = ops.pop();
        (lhs, rhs)
    }
}

ast_node! {
    /// A range expression: `a..b` (half-open) or `a..=b` (inclusive) — §4.11,
    /// ADR-059.
    ///
    /// Both bounds are required. There is no `a..`, `..b` or `..`: a range is a
    /// collection with a known length, and an open end has no length. The
    /// inclusiveness rides on the *operator token*, so `is_inclusive` is a question
    /// about the syntax and not a flag anyone has to keep in step.
    RangeExpr, RANGE_EXPR
}
impl RangeExpr {
    /// The operator token (`..` or `..=`).
    pub fn op(&self) -> Option<SyntaxToken> {
        token_matching(&self.syntax, |k| matches!(k, K::DOT2 | K::DOT2EQ))
    }

    /// Whether the upper bound is included (`..=`). A range whose operator token
    /// is missing — only reachable from a `PARSE_ERROR` subtree — reads as
    /// half-open, which is the form that cannot over-run its bound.
    pub fn is_inclusive(&self) -> bool {
        self.op().is_some_and(|t| t.kind() == K::DOT2EQ)
    }

    /// The lower and upper bound expressions.
    pub fn bounds(&self) -> (Option<Expr>, Option<Expr>) {
        let mut ops: Vec<_> = self
            .syntax
            .children()
            .filter_map(Expr::cast_from_child)
            .collect();
        let end = ops.pop();
        let start = ops.pop();
        (start, end)
    }
}

ast_node! {
    /// A unary operator expression `op x`.
    UnaryExpr, UNARY_EXPR
}
impl UnaryExpr {
    /// The operand expression.
    pub fn operand(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The operator token (`-`, `!`).
    pub fn op(&self) -> Option<SyntaxToken> {
        token_matching(&self.syntax, |k| matches!(k, K::MINUS | K::BANG))
    }
}

ast_node! {
    /// A parenthesized expression `( e )`.
    ParenExpr, PAREN_EXPR
}
impl ParenExpr {
    /// The inner expression.
    pub fn expr(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// A tuple expression `( e1, e2, … )`.
    TupleExpr, TUPLE_EXPR
}
impl TupleExpr {
    /// The tuple elements, in order.
    pub fn elements(&self) -> impl Iterator<Item = Expr> + '_ {
        self.syntax.children().filter_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// A list expression `[ e1, e2, … ]` — a `Vec` literal (§6.1).
    ListExpr, LIST_EXPR
}
impl ListExpr {
    /// The `[element, …]` list. `None` only for a malformed node.
    pub fn element_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }
    /// The element expressions, in source order. Empty for `[]`.
    pub fn elements(&self) -> Vec<Expr> {
        self.element_list()
            .map(|l| l.args().collect())
            .unwrap_or_default()
    }
}

ast_node! {
    /// A `{ stmt; …; expr }` block.
    BlockExpr, BLOCK_EXPR
}
impl BlockExpr {
    /// The statements/items of the block, in source order. A trailing expression
    /// is returned as an [`ExprStmt`] (which is how the parser represents it).
    pub fn stmts(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax.children()
    }
}

ast_node! {
    /// An `if cond { … } else { … }` expression.
    IfExpr, IF_EXPR
}
impl IfExpr {
    /// The condition expression (the first expression child).
    pub fn cond(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The then-branch block.
    pub fn then_branch(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
    /// The else branch (block or `else if`), if present.
    pub fn else_branch(&self) -> Option<ElseBranch> {
        child(&self.syntax)
    }
}

ast_node! {
    /// An `else { … }` or `else if …` arm.
    ElseBranch, ELSE_BRANCH
}
impl ElseBranch {
    /// The body of the else (a block, or a nested `if`).
    pub fn body(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// A `while cond { … }` expression.
    WhileExpr, WHILE_EXPR
}
impl WhileExpr {
    pub fn cond(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    pub fn body(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
}

ast_node! {
    /// `for name in iter { body }` (§4.11).
    ForExpr, FOR_EXPR
}
impl ForExpr {
    /// The binding pattern (`for x in …` → `x`, `for (k, v) in …` → `(k, v)`).
    ///
    /// A pattern and not a name token: a `for` binds one value per step, and
    /// taking that value apart is what a pattern is for. The overwhelmingly
    /// common shape is a single name, which is a [`PatternKind::Name`].
    pub fn binding(&self) -> Option<Pattern> {
        child(&self.syntax)
    }
    /// The iterator expression (`for x in iter`).
    pub fn iter(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    pub fn body(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
}

ast_node! {
    /// `loop { body }` (§4.11).
    LoopExpr, LOOP_EXPR
}
impl LoopExpr {
    pub fn body(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
}

ast_node! {
    /// `break [expr]` (§4.11).
    BreakExpr, BREAK_EXPR
}
impl BreakExpr {
    /// The optional break value (`break expr`).
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// `continue` (§4.11).
    ContinueExpr, CONTINUE_EXPR
}

ast_node! {
    /// `return [expr]` (§4.11).
    ReturnExpr, RETURN_EXPR
}
impl ReturnExpr {
    /// The optional return value (`return expr`).
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

ast_node! {
    /// A `callee(args)` call expression (covers `out(...)`).
    CallExpr, CALL_EXPR
}
impl CallExpr {
    /// The callee (a `PathExpr` naming the function/builtin). Present for a
    /// named call (`f(args)`); `None` for a postfix call on an arbitrary
    /// expression (`fs.get(0)(args)`), whose callee is an [`Expr`] — see
    /// [`callee_expr`](Self::callee_expr).
    pub fn callee(&self) -> Option<PathExpr> {
        child(&self.syntax)
    }
    /// The callee as an arbitrary expression, for a postfix call (`expr(args)`).
    /// `None` for a named call (use [`callee`](Self::callee) instead). This is
    /// the callee-not-a-path case (§4.10): calling a closure retrieved from
    /// a collection, the result of another call, etc.
    pub fn callee_expr(&self) -> Option<Expr> {
        // Only present when there is no `PathExpr` callee (a named call). The
        // first `Expr` child is then the postfix callee (a method call, paren,
        // prior call, …); the `ArgList` is not an `Expr` so it is skipped.
        if self.callee().is_some() {
            return None;
        }
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The argument list.
    pub fn arg_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }
    /// The written `[Type, …]` type arguments, for a constructor call that has
    /// them: `Counter[(Int, Int)]()`. `None` for every other call.
    pub fn type_args(&self) -> Option<TypeArgList> {
        child(&self.syntax)
    }
}

ast_node! {
    /// The `[Type, …]` type-argument list of a constructor call (§3.3).
    ///
    /// A sibling of the `ArgList` rather than part of the callee path, because it
    /// belongs to the *call*: `Counter` alone is still just a name, and the arguments
    /// say what the one call it heads constructs.
    TypeArgList, TYPE_ARG_LIST
}
impl TypeArgList {
    /// The type arguments, in source order.
    pub fn args(&self) -> impl Iterator<Item = TypeRef> + '_ {
        children(&self.syntax)
    }
}

ast_node! {
    /// A `read parser_expression` prefix expression (§7.1). Its single child is
    /// the parser expression applied to the whole process-input buffer.
    ReadExpr, READ_EXPR
}
impl ReadExpr {
    /// The parser expression body.
    pub fn parser_expr(&self) -> Option<ParserExpr> {
        child(&self.syntax)
    }
}

ast_node! {
    /// A `parse(text, parser_expression)` call (§7.1). The first child is the
    /// ordinary expression yielding the `Text`; the second is the parser expression.
    ParseExpr, PARSE_EXPR
}
impl ParseExpr {
    /// The `Text` expression to parse.
    pub fn text_expr(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The parser expression to apply.
    pub fn parser_expr(&self) -> Option<ParserExpr> {
        child(&self.syntax)
    }
}

ast_node! {
    /// A parser expression (§7 EBNF): an atomic, a template, or a constructor call.
    /// The body of `read` and the second argument of `parse`.
    ParserExpr, PARSER_EXPR
}
impl ParserExpr {
    /// Which kind of parser expression this is.
    pub fn kind(&self) -> ParserExprKind {
        for child in self.syntax.children() {
            match child.kind() {
                K::PARSER_ATOM => return ParserExprKind::Atom,
                K::PARSER_TEMPLATE => return ParserExprKind::Template,
                K::PARSER_CALL => return ParserExprKind::Call,
                _ => {}
            }
        }
        ParserExprKind::Unknown
    }

    /// The text of the parser-expression node (for atom names, template text,
    /// constructor names). The HIR reads this to build the `ParserAst`.
    ///
    /// For an atom (`PARSER_EXPR > PARSER_ATOM > Ident`), this returns the
    /// identifier's text. For a constructor call, the name lives in the nested
    /// `PATH_EXPR`; use [`ParserExpr::constructor_name`] for that.
    pub fn text(&self) -> Option<String> {
        use rowan::NodeOrToken;
        // Descend to the first token in the subtree (handles PARSER_ATOM nesting).
        for descendant in self.syntax.descendants_with_tokens() {
            if let NodeOrToken::Token(t) = descendant {
                if t.kind() == K::Ident {
                    return Some(t.text().to_string());
                }
            }
        }
        None
    }

    /// The constructor name for a `PARSER_CALL` parser expression (the text of
    /// the `PATH_EXPR`'s identifier). `None` for atoms/templates.
    pub fn constructor_name(&self) -> Option<String> {
        for child in self.syntax.children() {
            if child.kind() == K::PATH_EXPR {
                return Some(child.text().to_string());
            }
            // The call wraps a PARSER_CALL which contains the PATH_EXPR.
            for sub in child.descendants() {
                if sub.kind() == K::PATH_EXPR {
                    return Some(sub.text().to_string());
                }
            }
        }
        None
    }
}

/// The discriminant of a [`ParserExpr`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserExprKind {
    /// An atomic parser name (`int`, `char`, …).
    Atom,
    /// A backtick template.
    Template,
    /// A constructor call (`lines(P)`, …).
    Call,
    /// An unrecognized / malformed parser expression.
    Unknown,
}

ast_node! {
    /// A named argument inside a parser constructor call (§7.5):
    /// `name: parser_expr`. The name is the leading `Ident` token; the value is the
    /// nested [`ParserExpr`]. Used by heterogeneous `sections`
    /// (`rules: lines(...)`), `chars`/`grid` keyword args (`skip: whitespace`,
    /// `fill: value`), and as the `repeated(...)` tail marker of `sections`.
    ParserNamedArg, PARSER_NAMED_ARG
}
impl ParserNamedArg {
    /// The argument name (the leading `Ident`).
    pub fn name(&self) -> Option<String> {
        name_token(&self.syntax).map(|t| t.text().to_string())
    }

    /// The argument's parser-expression value.
    pub fn value(&self) -> Option<ParserExpr> {
        self.syntax.children().find_map(ParserExpr::cast)
    }

    /// The argument's **literal** value, if it has one: the raw token text of a
    /// `PARSER_KEYWORD_VALUE` child (`0`, `"-"`), quotes and all.
    ///
    /// A keyword argument's value is not a parser expression, so
    /// [`ParserNamedArg::value`] is `None` exactly when this is `Some` — the
    /// two are the grammar's two alternatives for what follows `name:`.
    /// Decoding a quoted value is `praxis_input_parser`'s job, so that both
    /// front ends get one answer from one place.
    pub fn keyword_value(&self) -> Option<String> {
        self.syntax
            .children()
            .find(|c| c.kind() == K::PARSER_KEYWORD_VALUE)
            .map(|c| c.text().to_string())
    }
}

ast_node! {
    /// A `receiver.method(args)` method-call expression (§16.2). The receiver
    /// is the first child expression; the method name is the `Ident` token after
    /// the `DOT`; the argument list follows.
    MethodCallExpr, METHOD_CALL_EXPR
}
impl MethodCallExpr {
    /// The receiver expression (`vec` in `vec.push(x)`).
    pub fn receiver(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The method name token (the `Ident` appearing after the `DOT`). Returns
    /// the first `Ident` that is not part of a child node (i.e. not the receiver
    /// path's name, which lives inside the `PATH_EXPR` child).
    pub fn method_name(&self) -> Option<SyntaxToken> {
        // The method name is the first `Ident` token that is a *direct* child of
        // this node — the receiver's name lives inside its own child node, so it
        // is not a direct token child here, which is exactly what `name_token`
        // walks.
        name_token(&self.syntax)
    }
    /// The argument list, if present.
    pub fn arg_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }
}

ast_node! {
    /// The `(arg, arg, …)` argument list of a call.
    ArgList, ARG_LIST
}
impl ArgList {
    /// The arguments, in order.
    pub fn args(&self) -> impl Iterator<Item = Expr> + '_ {
        self.syntax.children().filter_map(Expr::cast_from_child)
    }
}
