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

/// An `enum Name { Variant, Variant(Type), … }` declaration (M7, §4.6).
#[derive(Clone, Debug)]
pub struct EnumItem {
    syntax: SyntaxNode,
}
impl AstNode for EnumItem {
    const KIND: K = K::ENUM_ITEM;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// One variant of an enum declaration: `Name` or `Name(Type, …)` (M7, §4.6).
/// Named `EnumVariantNode` to avoid clashing with the type-system
/// `EnumVariantDef`.
#[derive(Clone, Debug)]
pub struct EnumVariantNode {
    syntax: SyntaxNode,
}
impl AstNode for EnumVariantNode {
    const KIND: K = K::ENUM_VARIANT;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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
    /// `for name in iter { body }` (M8, §4.11).
    For(ForExpr),
    /// `loop { body }` (M8, §4.11).
    Loop(LoopExpr),
    /// `break [expr]` (M8, §4.11).
    Break(BreakExpr),
    /// `continue` (M8, §4.11).
    Continue(ContinueExpr),
    /// `return [expr]` (M8, §4.11).
    Return(ReturnExpr),
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
    /// `match scrutinee { pattern => expr, … }` (M7, §4.6).
    Match(MatchExpr),
    /// `|params| expr` closure (M7, §4.10).
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
            Expr::Path(e) => e.syntax(),
            Expr::Bin(e) => e.syntax(),
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
            Expr::Read(e) => e.syntax(),
            Expr::Parse(e) => e.syntax(),
            Expr::RecordLit(e) => e.syntax(),
            Expr::FieldGet(e) => e.syntax(),
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
            K::PATH_EXPR => Expr::Path(PathExpr::from_syntax(n)),
            K::BIN_EXPR => Expr::Bin(BinExpr::from_syntax(n)),
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
            K::READ_EXPR => Expr::Read(ReadExpr::from_syntax(n)),
            K::PARSE_EXPR => Expr::Parse(ParseExpr::from_syntax(n)),
            K::RECORD_LIT_EXPR => Expr::RecordLit(RecordLitExpr::from_syntax(n)),
            K::FIELD_EXPR => Expr::FieldGet(FieldExpr::from_syntax(n)),
            K::MATCH_EXPR => Expr::Match(MatchExpr::from_syntax(n)),
            K::CLOSURE_EXPR => Expr::Closure(ClosureExpr::from_syntax(n)),
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

/// `match scrutinee { pattern => expr, … }` (M7, §4.6/§4.11).
#[derive(Clone, Debug)]
pub struct MatchExpr {
    syntax: SyntaxNode,
}
impl AstNode for MatchExpr {
    const KIND: K = K::MATCH_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// One `pattern => expr` arm of a match expression (M7, §4.6).
#[derive(Clone, Debug)]
pub struct MatchArm {
    syntax: SyntaxNode,
}
impl AstNode for MatchArm {
    const KIND: K = K::MATCH_ARM;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// `|params| expr` — a closure expression (M7, §4.10). The params are `PARAM`
/// children (no `PARAM_LIST` wrapper, since closures use `|…|` not `(…)`). The
/// body is the trailing expression child. Closures capture outer variables
/// automatically; the capture analysis lives in HIR.
#[derive(Clone, Debug)]
pub struct ClosureExpr {
    syntax: SyntaxNode,
}
impl AstNode for ClosureExpr {
    const KIND: K = K::CLOSURE_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
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

/// A pattern (M7, §4.6): `_`, literal, variable bind, or enum variant
/// (optionally with sub-patterns).
#[derive(Clone, Debug)]
pub struct Pattern {
    syntax: SyntaxNode,
}
impl AstNode for Pattern {
    const KIND: K = K::PATTERN;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl Pattern {
    /// The kind of this pattern, as a [`PatternKind`].
    pub fn kind(&self) -> PatternKind {
        let syntax = &self.syntax;
        // Check for wildcard.
        if syntax
            .children_with_tokens()
            .any(|e| matches!(e, rowan::NodeOrToken::Token(t) if t.kind() == K::UNDERSCORE))
        {
            return PatternKind::Wildcard;
        }
        // Check for a literal.
        if syntax
            .children_with_tokens()
            .any(|e| matches!(e, rowan::NodeOrToken::Token(t) if matches!(t.kind(), K::IntLit | K::TextLit | K::KW_TRUE | K::KW_FALSE)))
        {
            return PatternKind::Literal;
        }
        // Check for an Ident — variant or variable bind.
        let name_tok = syntax.children_with_tokens().find_map(|e| match e {
            rowan::NodeOrToken::Token(t) if t.kind() == K::Ident => Some(t),
            _ => None,
        });
        if let Some(tok) = name_tok {
            // If followed by `(`, it's a variant with sub-patterns.
            let has_parens = syntax.children().any(|c| c.kind() == K::PATTERN);
            if has_parens {
                return PatternKind::Variant(tok.text().to_string());
            }
            return PatternKind::Name(tok.text().to_string());
        }
        PatternKind::Wildcard
    }
    /// Sub-patterns (for a variant pattern `Number(n, _)`).
    pub fn sub_patterns(&self) -> impl Iterator<Item = Pattern> + '_ {
        self.syntax.children().filter_map(Pattern::cast)
    }
    /// The name token, if this is a variant or variable-bind pattern.
    pub fn name_token(&self) -> Option<SyntaxToken> {
        self.syntax.children_with_tokens().find_map(|e| match e {
            rowan::NodeOrToken::Token(t) if t.kind() == K::Ident => Some(t),
            _ => None,
        })
    }

    /// The literal token, if this is a literal pattern (`42`, `"hi"`, `true`,
    /// `false`). Used by the HIR lowerer to read the value the pattern tests
    /// against.
    pub fn literal_token(&self) -> Option<SyntaxToken> {
        self.syntax.children_with_tokens().find_map(|e| match e {
            rowan::NodeOrToken::Token(t)
                if matches!(t.kind(), K::IntLit | K::TextLit | K::KW_TRUE | K::KW_FALSE) =>
            {
                Some(t)
            }
            _ => None,
        })
    }
}

/// What kind of pattern a [`Pattern`] is (M7, §4.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternKind {
    /// `_` — matches anything.
    Wildcard,
    /// A literal: `42`, `"hi"`, `true`, `false`.
    Literal,
    /// A variable bind: `x` — matches anything and binds the value to `x`.
    Name(String),
    /// An enum variant: `Empty` or `Number(…)` — the string is the variant name.
    Variant(String),
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

/// `for name in iter { body }` (M8, §4.11).
#[derive(Clone, Debug)]
pub struct ForExpr {
    syntax: SyntaxNode,
}
impl AstNode for ForExpr {
    const KIND: K = K::FOR_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ForExpr {
    /// The binding name token (`for x in …` → `x`).
    pub fn binding(&self) -> Option<SyntaxToken> {
        self.syntax.children_with_tokens().find_map(|e| {
            let t = e.into_token()?;
            (t.kind() == K::Ident).then_some(t)
        })
    }
    /// The iterator expression (`for x in iter`).
    pub fn iter(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
    pub fn body(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
}

/// `loop { body }` (M8, §4.11).
#[derive(Clone, Debug)]
pub struct LoopExpr {
    syntax: SyntaxNode,
}
impl AstNode for LoopExpr {
    const KIND: K = K::LOOP_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl LoopExpr {
    pub fn body(&self) -> Option<BlockExpr> {
        child(&self.syntax)
    }
}

/// `break [expr]` (M8, §4.11).
#[derive(Clone, Debug)]
pub struct BreakExpr {
    syntax: SyntaxNode,
}
impl AstNode for BreakExpr {
    const KIND: K = K::BREAK_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl BreakExpr {
    /// The optional break value (`break expr`).
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
    }
}

/// `continue` (M8, §4.11).
#[derive(Clone, Debug)]
pub struct ContinueExpr {
    syntax: SyntaxNode,
}
impl AstNode for ContinueExpr {
    const KIND: K = K::CONTINUE_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

/// `return [expr]` (M8, §4.11).
#[derive(Clone, Debug)]
pub struct ReturnExpr {
    syntax: SyntaxNode,
}
impl AstNode for ReturnExpr {
    const KIND: K = K::RETURN_EXPR;
    fn from_syntax(syntax: SyntaxNode) -> Self {
        Self { syntax }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}
impl ReturnExpr {
    /// The optional return value (`return expr`).
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast_from_child)
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
