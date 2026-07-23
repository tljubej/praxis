//! The typed syntax-node wrappers M2 consumes (ADR-009).
//!
//! Each is a thin newtype over [`SyntaxNode`] with strongly-typed accessors. They
//! are intentionally minimal: only nodes walked by name resolution or type
//! inference appear here. The pattern is the rust-analyzer/rowan idiom — `cast`
//! by kind, `syntax` for the borrow, accessors that find the relevant child.

use praxis_syntax::{SyntaxKind as K, SyntaxNode, SyntaxToken};

use crate::{child, children, name_token, AstNode};

// ---------------------------------------------------------------------------
// Root + items
// ---------------------------------------------------------------------------

/// The root of a parsed file. Children are statements / `fn` items.
#[derive(Clone, Debug)]
pub struct SourceFile {
    syntax: SyntaxNode,
}
impl AstNode for SourceFile {
    const KIND: K = K::SOURCE_FILE;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl SourceFile {
    /// The top-level statements and items, in source order. Items are returned
    /// generically as nodes; callers downcast via the wrapper [`cast`](crate::AstNode::cast).
    pub fn stmts(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax.children()
    }
}

/// A `let name = expr` binding.
#[derive(Clone, Debug)]
pub struct LetStmt {
    syntax: SyntaxNode,
}
impl AstNode for LetStmt {
    const KIND: K = K::LET_STMT;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl LetStmt {
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

/// A `var name = expr` binding.
#[derive(Clone, Debug)]
pub struct VarStmt {
    syntax: SyntaxNode,
}
impl AstNode for VarStmt {
    const KIND: K = K::VAR_STMT;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl VarStmt {
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    pub fn ty(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
    pub fn init(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

/// A `name = expr` / `name += expr` reassignment.
#[derive(Clone, Debug)]
pub struct AssignStmt {
    syntax: SyntaxNode,
}
impl AstNode for AssignStmt {
    const KIND: K = K::ASSIGN_STMT;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl AssignStmt {
    /// The reassigned name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The assignment operator token (`=`, `+=`, …).
    pub fn op(&self) -> Option<SyntaxToken> {
        use rowan::NodeOrToken;
        self.syntax.children_with_tokens().find_map(|e| match e {
            NodeOrToken::Token(t)
                if matches!(
                    t.kind(),
                    K::EQ | K::PLUS_EQ | K::MINUS_EQ | K::STAR_EQ | K::SLASH_EQ | K::PERCENT_EQ
                ) =>
            {
                Some(t)
            }
            _ => None,
        })
    }
    /// The right-hand-side expression.
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

/// A top-level or nested `fn name(params) -> Ret { body }`.
#[derive(Clone, Debug)]
pub struct FnItem {
    syntax: SyntaxNode,
}
impl AstNode for FnItem {
    const KIND: K = K::FN_ITEM;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// The `(...)` parameter list.
#[derive(Clone, Debug)]
pub struct ParamList {
    syntax: SyntaxNode,
}
impl AstNode for ParamList {
    const KIND: K = K::PARAM_LIST;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ParamList {
    /// The parameters, in order.
    pub fn params(&self) -> impl Iterator<Item = Param> + '_ {
        children::<Param>(&self.syntax)
    }
}

/// A single `name: Type` parameter.
#[derive(Clone, Debug)]
pub struct Param {
    syntax: SyntaxNode,
}
impl AstNode for Param {
    const KIND: K = K::PARAM;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl Param {
    /// The parameter name.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
    /// The declared parameter type.
    pub fn ty(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
}

/// A bare expression used as a statement (including trailing exprs in a block).
#[derive(Clone, Debug)]
pub struct ExprStmt {
    syntax: SyntaxNode,
}
impl AstNode for ExprStmt {
    const KIND: K = K::EXPR_STMT;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ExprStmt {
    /// The contained expression.
    pub fn expr(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

// ---------------------------------------------------------------------------
// Type references
// ---------------------------------------------------------------------------

/// A type written in source: a scalar name, a tuple, or a function type.
#[derive(Clone, Debug)]
pub struct TypeRef {
    syntax: SyntaxNode,
}
impl AstNode for TypeRef {
    const KIND: K = K::TYPE_REF;
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
    Path(PathExpr),
    Bin(BinExpr),
    Unary(UnaryExpr),
    Paren(ParenExpr),
    Block(BlockExpr),
    If(IfExpr),
    While(WhileExpr),
    Call(CallExpr),
    Tuple(TupleExpr),
    /// An unparseable expression the parser wrapped in a `PARSE_ERROR` node.
    Error(SyntaxNode),
}

impl Expr {
    /// Try to cast a child node into an `Expr` of any known expression kind.
    fn cast_from_child(n: SyntaxNode) -> Option<Expr> {
        Some(match n.kind() {
            K::LITERAL => Expr::Literal(Literal::from_syntax(n)),
            K::PATH_EXPR => Expr::Path(PathExpr::from_syntax(n)),
            K::BIN_EXPR => Expr::Bin(BinExpr::from_syntax(n)),
            K::UNARY_EXPR => Expr::Unary(UnaryExpr::from_syntax(n)),
            K::PAREN_EXPR => Expr::Paren(ParenExpr::from_syntax(n)),
            K::BLOCK_EXPR => Expr::Block(BlockExpr::from_syntax(n)),
            K::IF_EXPR => Expr::If(IfExpr::from_syntax(n)),
            K::WHILE_EXPR => Expr::While(WhileExpr::from_syntax(n)),
            K::CALL_EXPR => Expr::Call(CallExpr::from_syntax(n)),
            K::TUPLE_EXPR => Expr::Tuple(TupleExpr::from_syntax(n)),
            K::PARSE_ERROR => Expr::Error(n),
            _ => return None,
        })
    }
}

/// A literal: `IntLit`, `TextLit`, `true`/`false`, backtick template.
#[derive(Clone, Debug)]
pub struct Literal {
    syntax: SyntaxNode,
}
impl AstNode for Literal {
    const KIND: K = K::LITERAL;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl Literal {
    /// The single literal token.
    pub fn token(&self) -> Option<SyntaxToken> {
        use rowan::NodeOrToken;
        self.syntax
            .children_with_tokens()
            .filter_map(|e| match e {
                NodeOrToken::Token(t) => Some(t),
                NodeOrToken::Node(_) => None,
            })
            .find(|t| {
                matches!(
                    t.kind(),
                    K::IntLit | K::TextLit | K::BacktickTemplate | K::KW_TRUE | K::KW_FALSE
                )
            })
    }
}

/// A name used as a value, or a callee (a bare `Ident` token, possibly followed
/// by a call — the call wrapper wraps the path).
#[derive(Clone, Debug)]
pub struct PathExpr {
    syntax: SyntaxNode,
}
impl AstNode for PathExpr {
    const KIND: K = K::PATH_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl PathExpr {
    /// The name being referenced.
    pub fn name(&self) -> Option<SyntaxToken> {
        name_token(&self.syntax)
    }
}

/// A binary operator expression `a op b`.
#[derive(Clone, Debug)]
pub struct BinExpr {
    syntax: SyntaxNode,
}
impl AstNode for BinExpr {
    const KIND: K = K::BIN_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl BinExpr {
    /// The operator token (`+`, `==`, …).
    pub fn op(&self) -> Option<SyntaxToken> {
        use rowan::NodeOrToken;
        self.syntax.children_with_tokens().find_map(|e| match e {
            NodeOrToken::Token(t)
                if matches!(
                    t.kind(),
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
                ) =>
            {
                Some(t)
            }
            _ => None,
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

/// A unary operator expression `op x`.
#[derive(Clone, Debug)]
pub struct UnaryExpr {
    syntax: SyntaxNode,
}
impl AstNode for UnaryExpr {
    const KIND: K = K::UNARY_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl UnaryExpr {
    /// The operand expression.
    pub fn operand(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    /// The operator token (`-`, `!`).
    pub fn op(&self) -> Option<SyntaxToken> {
        use rowan::NodeOrToken;
        self.syntax.children_with_tokens().find_map(|e| match e {
            NodeOrToken::Token(t) if matches!(t.kind(), K::MINUS | K::BANG) => Some(t),
            _ => None,
        })
    }
}

/// A parenthesized expression `( e )`.
#[derive(Clone, Debug)]
pub struct ParenExpr {
    syntax: SyntaxNode,
}
impl AstNode for ParenExpr {
    const KIND: K = K::PAREN_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ParenExpr {
    /// The inner expression.
    pub fn expr(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

/// A tuple expression `( e1, e2, … )`.
#[derive(Clone, Debug)]
pub struct TupleExpr {
    syntax: SyntaxNode,
}
impl AstNode for TupleExpr {
    const KIND: K = K::TUPLE_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl TupleExpr {
    /// The tuple elements, in order.
    pub fn elements(&self) -> impl Iterator<Item = Expr> + '_ {
        self.syntax.children().filter_map(Expr::cast_from_child)
    }
}

/// A `{ stmt; …; expr }` block.
#[derive(Clone, Debug)]
pub struct BlockExpr {
    syntax: SyntaxNode,
}
impl AstNode for BlockExpr {
    const KIND: K = K::BLOCK_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl BlockExpr {
    /// The statements/items of the block, in source order. A trailing expression
    /// is returned as an [`ExprStmt`] (which is how the parser represents it).
    pub fn stmts(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax.children()
    }
}

/// An `if cond { … } else { … }` expression.
#[derive(Clone, Debug)]
pub struct IfExpr {
    syntax: SyntaxNode,
}
impl AstNode for IfExpr {
    const KIND: K = K::IF_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// An `else { … }` or `else if …` arm.
#[derive(Clone, Debug)]
pub struct ElseBranch {
    syntax: SyntaxNode,
}
impl AstNode for ElseBranch {
    const KIND: K = K::ELSE_BRANCH;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ElseBranch {
    /// The body of the else (a block, or a nested `if`).
    pub fn body(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

/// A `while cond { … }` expression.
#[derive(Clone, Debug)]
pub struct WhileExpr {
    syntax: SyntaxNode,
}
impl AstNode for WhileExpr {
    const KIND: K = K::WHILE_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl WhileExpr {
    pub fn cond(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    pub fn body(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
}

/// A `callee(args)` call expression (covers `out(...)`).
#[derive(Clone, Debug)]
pub struct CallExpr {
    syntax: SyntaxNode,
}
impl AstNode for CallExpr {
    const KIND: K = K::CALL_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl CallExpr {
    /// The callee (a `PathExpr` naming the function/builtin).
    pub fn callee(&self) -> Option<PathExpr> {
        child(&self.syntax)
    }
    /// The argument list.
    pub fn arg_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }
}

/// The `(arg, arg, …)` argument list of a call.
#[derive(Clone, Debug)]
pub struct ArgList {
    syntax: SyntaxNode,
}
impl AstNode for ArgList {
    const KIND: K = K::ARG_LIST;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ArgList {
    /// The arguments, in order.
    pub fn args(&self) -> impl Iterator<Item = Expr> + '_ {
        self.syntax.children().filter_map(Expr::cast_from_child)
    }
}
