//! Completion (WS5, §15.2's context list).
//!
//! Two halves: [`context_at`] decides *what kind of place* the cursor is in, and
//! [`items`] turns that into a list. The split matters because the first half is
//! the one that has to work on text that does not parse — `grid.` is not an
//! expression — and the second half is a rendering of tables the compiler
//! already owns (`builtin_catalog`, `AtomicKind::ALL`, `Constructor::ALL`).
//!
//! [`trigger_answers_here`] sits between them and answers a different question:
//! not *what* to offer but *whether the editor was right to ask*.
//!
//! **Nothing here carries a second list of names.** A method comes from
//! `praxis_stdlib::completion::completion_data`, generated from the catalog the
//! compiler dispatches through; an atomic from `AtomicKind::ALL`; a constructor
//! and its keyword argument from `Constructor::ALL`/`keyword_arg`. That is what
//! makes "a constructor added later is offered" true by construction.

use lsp_types::{CompletionItem, CompletionItemKind, Documentation, InsertTextFormat};
use praxis_hir::{ParserMode, SymbolKind};
use praxis_input_parser::{AtomicKind, Constructor};
use praxis_syntax::{SyntaxKind, SyntaxNode};
use praxis_types::{data::TypeData, Type};

use crate::query::{CompletionContext, Snapshot};

/// Decide the completion context at `offset`.
///
/// The order of the tests is the order of specificity: a `.` beats everything,
/// then the parser sublanguage, then a record literal, then a match pattern,
/// then the lexical fallback.
#[must_use]
pub fn context_at(snapshot: &Snapshot, offset: u32) -> CompletionContext {
    let prefix = word_prefix(snapshot.source_text(), offset);

    if let Some(receiver) = receiver_before_dot(snapshot, offset) {
        return CompletionContext::Dot { receiver, prefix };
    }
    if let Some(ctx) = parser_context(snapshot, offset, &prefix) {
        return ctx;
    }
    if let Some(ctx) = record_field_context(snapshot, offset, &prefix) {
        return ctx;
    }
    if let Some(ctx) = enum_case_context(snapshot, offset, &prefix) {
        return ctx;
    }
    CompletionContext::Lexical { prefix }
}

/// Whether a menu the editor opened *because a character was typed* belongs
/// here.
///
/// A trigger character is a promise the editor keeps literally: it fires the
/// instant that character is typed, wherever it is typed. `.` can afford that,
/// because a `.` in Praxis is always a member access. The template characters
/// cannot. They are registered for the parser sublanguage, where `` `{n:int}` ``
/// needs a menu over text that is not yet an expression — but the same
/// characters carry ordinary meanings outside a template, and `{` carries the
/// most ordinary one there is: it opens every block. `fn main() {`, `if x {`,
/// `match d {` each resolved to [`CompletionContext::Lexical`] with an empty
/// prefix, so the editor was handed every name in the file, pre-selected, at the
/// exact moment the user was about to *invent* a name — and an editor that
/// accepts a selected row on <kbd>Enter</kbd> then commits the first one.
///
/// So each character answers only where it means what it was registered for.
/// The gate is on trigger characters **alone**: an explicit request
/// (<kbd>Ctrl</kbd>+<kbd>Space</kbd>) and the editor's own suggest-as-you-type
/// both arrive as `INVOKED`, and neither is ever suppressed — the lexical list
/// after `{` is still one keystroke away, which is the difference between a
/// menu offered and a menu imposed.
#[must_use]
pub fn trigger_answers_here(trigger: &str, ctx: &CompletionContext) -> bool {
    match trigger {
        "." => matches!(ctx, CompletionContext::Dot { .. }),
        "`" | ":" => matches!(ctx, CompletionContext::Parser { .. }),
        // `Name {` is the one place outside a template where a brace introduces
        // a closed list of names, and offering a record's own fields there is
        // the reason `{` is worth registering at all.
        "{" => matches!(
            ctx,
            CompletionContext::Parser { .. } | CompletionContext::RecordFields { .. }
        ),
        // Not a character this server registered. Some other client's idea of a
        // trigger is not this function's to veto.
        _ => true,
    }
}

/// The identifier characters immediately before `offset` — what the user has
/// typed so far of the name they are completing.
fn word_prefix(text: &str, offset: u32) -> String {
    let end = (offset as usize).min(text.len());
    let bytes = &text[..end];
    let start = bytes
        .char_indices()
        .rev()
        .take_while(|(_, c)| praxis_syntax::ident::is_ident_continue(*c))
        .last()
        .map_or(end, |(i, _)| i);
    bytes[start..].to_string()
}

/// The receiver type when the cursor is after a `.`.
///
/// §8's measurement, applied: walk left from the cursor to a `DOT`, take the
/// **complete** expression node the parser left immediately before it, and read
/// the type inference already recorded for that node. `grid.` parses as
/// `EXPR_STMT [ PATH_EXPR "grid", DOT ]` — the postfix loop breaks, the
/// checkpoint never becomes a node, and the receiver is nonetheless whole. No
/// parser recovery and no speculative edit is needed, which is why WS5 does not
/// open by rewriting recovery.
fn receiver_before_dot(snapshot: &Snapshot, offset: u32) -> Option<Type> {
    let token = snapshot.token_before(offset)?;
    let dot = if token.kind() == SyntaxKind::DOT {
        token
    } else {
        // `grid.le|` — the cursor is in a partial member name; the dot is one
        // non-trivia token further left.
        let mut prev = token.prev_token()?;
        while prev.kind().is_trivia() {
            prev = prev.prev_token()?;
        }
        if prev.kind() != SyntaxKind::DOT {
            return None;
        }
        prev
    };
    let receiver = crate::query::expr_before_dot(&dot)?;
    type_of_node(snapshot, &receiver)
}

/// The recorded type of `node`, or of its last typed descendant.
///
/// A `PATH_EXPR` has a type; a `PAREN_EXPR` around it may not, and neither does
/// an `EXPR_STMT`. Descending to the last child that does is what makes
/// `(a + b).` work.
fn type_of_node(snapshot: &Snapshot, node: &SyntaxNode) -> Option<Type> {
    let analysis = snapshot.analyze();
    if let Some(t) = analysis.expr_types.get(&praxis_hir::NodeKey::of(node)) {
        return Some(*t);
    }
    node.children()
        .filter_map(|c| type_of_node(snapshot, &c))
        .last()
}

/// The parser-sublanguage context, when the cursor is inside a `read`/`parse`
/// body.
fn parser_context(snapshot: &Snapshot, offset: u32, prefix: &str) -> Option<CompletionContext> {
    let inside_parser_syntax = snapshot
        .token_at(offset)
        .map(|t| {
            t.parent_ancestors().any(|n| {
                matches!(
                    n.kind(),
                    SyntaxKind::READ_EXPR | SyntaxKind::PARSE_EXPR | SyntaxKind::PARSER_EXPR
                )
            })
        })
        .unwrap_or(false);
    if !inside_parser_syntax {
        return None;
    }

    // The compiler's own answer when it has one. A body that is mid-edit does
    // not convert, so there is no index and no mode to read — and rather than
    // re-deriving where the capture ends (ADR-098's whole point), the fallback
    // simply does not claim to know: it answers `Expression`, whose item list is
    // the same one a capture body takes (D10).
    let mode = snapshot
        .input_parser_at(offset)
        .map_or(ParserMode::Expression, |idx| idx.mode_at(offset));

    Some(CompletionContext::Parser {
        mode,
        enclosing: enclosing_constructor(snapshot, offset),
        prefix: prefix.to_string(),
    })
}

/// The constructor whose argument list the cursor sits in, read from its name
/// token through `Constructor::from_keyword`.
fn enclosing_constructor(snapshot: &Snapshot, offset: u32) -> Option<Constructor> {
    let arg_list = snapshot.ancestor_of_kind(offset, SyntaxKind::PARSER_ARG_LIST)?;
    let call = arg_list.parent()?;
    let name = call
        .children()
        .find(|c| c.kind() == SyntaxKind::PATH_EXPR)?
        .text()
        .to_string();
    Constructor::from_keyword(name.trim())
}

fn record_field_context(
    snapshot: &Snapshot,
    offset: u32,
    prefix: &str,
) -> Option<CompletionContext> {
    let lit = snapshot.ancestor_of_kind(offset, SyntaxKind::RECORD_LIT_EXPR)?;
    let record = type_of_node(snapshot, &lit)?;
    Some(CompletionContext::RecordFields {
        record,
        prefix: prefix.to_string(),
    })
}

fn enum_case_context(snapshot: &Snapshot, offset: u32, prefix: &str) -> Option<CompletionContext> {
    // Only inside a pattern: the arm's *body* is an ordinary expression.
    snapshot.ancestor_of_kind(offset, SyntaxKind::PATTERN)?;
    let match_expr = snapshot.ancestor_of_kind(offset, SyntaxKind::MATCH_EXPR)?;
    // The scrutinee is the match's first expression child, before the arms.
    let scrutinee = match_expr
        .children()
        .find(|c| c.kind() != SyntaxKind::MATCH_ARM)?;
    let ty = type_of_node(snapshot, &scrutinee)?;
    Some(CompletionContext::EnumCases {
        scrutinee: ty,
        prefix: prefix.to_string(),
    })
}

/// The completion list for a context.
#[must_use]
pub fn items(snapshot: &Snapshot, ctx: &CompletionContext) -> Vec<CompletionItem> {
    let mut out = match ctx {
        CompletionContext::Dot { receiver, .. } => dot_items(snapshot, *receiver),
        CompletionContext::Parser { enclosing, .. } => parser_items(*enclosing),
        CompletionContext::RecordFields { record, .. } => field_items(snapshot, *record),
        CompletionContext::EnumCases { scrutinee, .. } => variant_items(snapshot, *scrutinee),
        CompletionContext::Lexical { .. } => lexical_items(snapshot),
    };
    let prefix = ctx.prefix();
    if !prefix.is_empty() {
        out.retain(|item| item.label.starts_with(prefix));
    }
    out
}

/// Receiver methods **and** record fields after a `.`.
///
/// The methods come from `completion_data`, which M8 generated from
/// `builtin_catalog()` and gated 1:1 against it — so the signatures shown are
/// the ones the compiler dispatches on, and criterion 2's "with signatures" is
/// `params`/`result` rather than a second table.
fn dot_items(snapshot: &Snapshot, receiver: Type) -> Vec<CompletionItem> {
    let analysis = snapshot.analyze();
    let db = &analysis.db;
    let mut out = Vec::new();

    // Fields first: a record's own members are nearer than any method.
    out.extend(field_items(snapshot, receiver));

    let Some(pattern) = praxis_hir::catalog::type_to_pattern(db, receiver) else {
        return out;
    };
    let catalog = praxis_stdlib::builtin_catalog();
    for entry in catalog.entries() {
        // The *same function* dispatch uses, not the same rule restated
        // (ADR-127 decision 1). This used to be a local copy with the LSP's own
        // comment admitting it — "the same rule … restated" — and the two are
        // filtered differently only in that dispatch also asks name and arity,
        // which completion must not.
        if !praxis_stdlib::pattern_matches(&entry.receiver, &pattern) {
            continue;
        }
        if is_operator(entry.name) {
            continue;
        }
        let params: Vec<String> = entry.params.iter().map(ToString::to_string).collect();
        let signature = format!("({}) -> {}", params.join(", "), entry.result);
        out.push(CompletionItem {
            label: entry.name.to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some(signature),
            documentation: Some(Documentation::String(entry.doc.to_string())),
            insert_text: Some(format!("{}(", entry.name)),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..CompletionItem::default()
        });
    }
    out
}

/// The catalog's **operator** rows, which are not names a user can type after a
/// `.`.
///
/// `v[0]`, `m[k] = x` and `best[k] max= s` are catalog entries because dispatch
/// goes through the catalog for them too (§6.2, REP-16/REP-21) — their names are
/// `[]`, `[]=`, `[]min=` and `[]max=`. Offering them as completions would put
/// `grid.[]` in the list, which is not syntax.
fn is_operator(name: &str) -> bool {
    use praxis_stdlib::catalog::{INDEX_READ, INDEX_STORE, INDEX_STORE_MAX, INDEX_STORE_MIN};
    matches!(
        name,
        INDEX_READ | INDEX_STORE | INDEX_STORE_MIN | INDEX_STORE_MAX
    )
}

fn field_items(snapshot: &Snapshot, ty: Type) -> Vec<CompletionItem> {
    let db = &snapshot.analyze().db;
    let TypeData::Record { def, .. } = db.data(db.follow(ty)) else {
        return Vec::new();
    };
    db.record_def(*def)
        .fields
        .iter()
        .map(|f| CompletionItem {
            label: f.name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(db.render(db.follow(f.ty))),
            ..CompletionItem::default()
        })
        .collect()
}

fn variant_items(snapshot: &Snapshot, ty: Type) -> Vec<CompletionItem> {
    let db = &snapshot.analyze().db;
    let TypeData::Enum { def, .. } = db.data(db.follow(ty)) else {
        return Vec::new();
    };
    db.enum_def(*def)
        .variants
        .iter()
        .map(|v| {
            let detail = if v.payload.is_empty() {
                v.name.clone()
            } else {
                let payload: Vec<String> =
                    v.payload.iter().map(|p| db.render(db.follow(*p))).collect();
                format!("{}({})", v.name, payload.join(", "))
            };
            CompletionItem {
                label: v.name.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(detail),
                ..CompletionItem::default()
            }
        })
        .collect()
}

/// §7.4's atomics, §7.5's constructors, and the enclosing constructor's own
/// named argument and flag.
fn parser_items(enclosing: Option<Constructor>) -> Vec<CompletionItem> {
    let mut out: Vec<CompletionItem> = AtomicKind::ALL
        .iter()
        .map(|a| CompletionItem {
            label: a.keyword().to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("input parser atomic (§7.4)".to_string()),
            ..CompletionItem::default()
        })
        .collect();

    out.extend(Constructor::ALL.iter().map(|c| CompletionItem {
        label: c.keyword().to_string(),
        kind: Some(CompletionItemKind::FUNCTION),
        detail: Some(crate::signature::constructor_signature(*c)),
        insert_text: Some(format!("{}(", c.keyword())),
        ..CompletionItem::default()
    }));

    if let Some(ctor) = enclosing {
        // **From the constructor, not from a name list.** `chars`'s `skip:` and
        // `grid`'s `fill:` are the constructor's own question; a hard-coded list
        // is the defect `Constructor::keyword_arg` exists to prevent.
        if let Some(kw) = ctor.keyword_arg() {
            out.push(CompletionItem {
                label: format!("{kw}:"),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some(format!("`{}`'s keyword argument (§7.5)", ctor.keyword())),
                ..CompletionItem::default()
            });
        }
        if ctor == Constructor::Grid {
            out.push(CompletionItem {
                label: RAGGED_FLAG.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some("permit uneven rows, padded with `fill:` (§7.5)".to_string()),
                ..CompletionItem::default()
            });
        }
    }
    out
}

/// The one bare-keyword argument in §7.5. Named here so the semantic-token
/// layer and the completion layer spell it the same way.
pub(crate) const RAGGED_FLAG: &str = "ragged";

/// Lexical identifiers visible at the cursor.
///
/// Approximate by design, and approximate in the safe direction: a top-level
/// declaration is always offered, and a local is offered when the cursor is
/// inside the block its declaration is in. The scope tree is keyed by
/// `ScopeId`, not by span, so "which scope is this offset in" is not a question
/// it can answer; the containing-block test is the tree's answer to the same
/// question and it agrees with the resolver wherever a name resolves at all.
fn lexical_items(snapshot: &Snapshot) -> Vec<CompletionItem> {
    let analysis = snapshot.analyze();
    let db = &analysis.db;
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for sym in analysis.names.all() {
        let kind = match sym.kind {
            SymbolKind::Fn => CompletionItemKind::FUNCTION,
            SymbolKind::Struct | SymbolKind::Enum | SymbolKind::BuiltinType => {
                CompletionItemKind::STRUCT
            }
            SymbolKind::EnumVariant => CompletionItemKind::ENUM_MEMBER,
            SymbolKind::Param => CompletionItemKind::VARIABLE,
            SymbolKind::Builtin => CompletionItemKind::FUNCTION,
            SymbolKind::Var => CompletionItemKind::VARIABLE,
        };
        // One entry per name: two shadowed bindings offer the same word, and a
        // list that repeats it says nothing the first entry did not.
        if !seen.insert(sym.name.clone()) {
            continue;
        }
        out.push(CompletionItem {
            label: sym.name.clone(),
            kind: Some(kind),
            detail: sym.scheme.as_ref().map(|s| db.render_scheme(s)),
            ..CompletionItem::default()
        });
    }
    out
}
