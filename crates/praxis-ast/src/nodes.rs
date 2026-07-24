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

/// A `struct Name { field: Type, … }` declaration (M7, §4.5).
#[derive(Clone, Debug)]
pub struct StructItem {
    syntax: SyntaxNode,
}
impl AstNode for StructItem {
    const KIND: K = K::STRUCT_ITEM;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// The `{ field: Type, … }` body of a struct, or the `{ field: expr, … }` body
/// of a record literal. Reused for both declaration types and record-literal
/// expressions (M7).
#[derive(Clone, Debug)]
pub struct FieldList {
    syntax: SyntaxNode,
}
impl AstNode for FieldList {
    const KIND: K = K::FIELD_LIST;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl FieldList {
    /// The fields in declaration order.
    pub fn fields(&self) -> impl Iterator<Item = Field> + '_ {
        self.syntax.children().filter_map(Field::cast)
    }
}

/// A single `name: Type` field (in a struct) or `name: expr` / `name` (pun, in
/// a record literal). M7, §4.5.
#[derive(Clone, Debug)]
pub struct Field {
    syntax: SyntaxNode,
}
impl AstNode for Field {
    const KIND: K = K::FIELD;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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
    MethodCall(MethodCallExpr),
    Tuple(TupleExpr),
    /// `read parser_expression` (§7.1, M6).
    Read(ReadExpr),
    /// `parse(text, parser_expression)` (§7.1, M6).
    Parse(ParseExpr),
    /// `Name { field: expr, … }` record literal (M7, §4.5).
    RecordLit(RecordLitExpr),
    /// `receiver.field` field access (M7, §4.5).
    FieldGet(FieldExpr),
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
            K::METHOD_CALL_EXPR => Expr::MethodCall(MethodCallExpr::from_syntax(n)),
            K::TUPLE_EXPR => Expr::Tuple(TupleExpr::from_syntax(n)),
            K::READ_EXPR => Expr::Read(ReadExpr::from_syntax(n)),
            K::PARSE_EXPR => Expr::Parse(ParseExpr::from_syntax(n)),
            K::RECORD_LIT_EXPR => Expr::RecordLit(RecordLitExpr::from_syntax(n)),
            K::FIELD_EXPR => Expr::FieldGet(FieldExpr::from_syntax(n)),
            K::PARSE_ERROR => Expr::Error(n),
            _ => return None,
        })
    }
}

/// `Name { field: expr, … }` — a record-literal expression (M7, §4.5). The
/// first child is the `PATH_EXPR` naming the struct type; the `FIELD_LIST` holds
/// the field initializers (explicit `name: expr` or punned `name`).
#[derive(Clone, Debug)]
pub struct RecordLitExpr {
    syntax: SyntaxNode,
}
impl AstNode for RecordLitExpr {
    const KIND: K = K::RECORD_LIT_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// `receiver.field` — field access (M7, §4.5). The first child is the receiver
/// expression; the field name is the trailing `Ident` token.
#[derive(Clone, Debug)]
pub struct FieldExpr {
    syntax: SyntaxNode,
}
impl AstNode for FieldExpr {
    const KIND: K = K::FIELD_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// A `read parser_expression` prefix expression (§7.1, M6). Its single child is
/// the parser expression applied to the whole process-input buffer.
#[derive(Clone, Debug)]
pub struct ReadExpr {
    syntax: SyntaxNode,
}
impl AstNode for ReadExpr {
    const KIND: K = K::READ_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ReadExpr {
    /// The parser expression body.
    pub fn parser_expr(&self) -> Option<ParserExpr> {
        child(&self.syntax)
    }
}

/// A `parse(text, parser_expression)` call (§7.1, M6). The first child is the
/// ordinary expression yielding the `Text`; the second is the parser expression.
#[derive(Clone, Debug)]
pub struct ParseExpr {
    syntax: SyntaxNode,
}
impl AstNode for ParseExpr {
    const KIND: K = K::PARSE_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// A parser expression (§7 EBNF): an atomic, a template, or a constructor call.
/// The body of `read` and the second argument of `parse`.
#[derive(Clone, Debug)]
pub struct ParserExpr {
    syntax: SyntaxNode,
}
impl AstNode for ParserExpr {
    const KIND: K = K::PARSER_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// A `receiver.method(args)` method-call expression (M5, §16.2). The receiver
/// is the first child expression; the method name is the `Ident` token after
/// the `DOT`; the argument list follows.
#[derive(Clone, Debug)]
pub struct MethodCallExpr {
    syntax: SyntaxNode,
}
impl AstNode for MethodCallExpr {
    const KIND: K = K::METHOD_CALL_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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
        // Walk tokens; the method name is the first `Ident` token that is a
        // *direct* child of this node (the receiver's name lives inside its own
        // child node, so it is not a direct token child here).
        self.syntax
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == K::Ident)
    }
    /// The argument list, if present.
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
