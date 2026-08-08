//! The shared front-end query layer (§14.2, ADR-097).
//!
//! §14.2 is one sentence with teeth: "The CLI and LSP must share the same
//! front-end query API." This module is that API. `praxis check` calls it, the
//! server calls it, and the sort order, the "analyze even when parsing reported"
//! decision and the diagnostic set are stated **once**, here — so a divergence
//! between what `praxis check` prints and what the editor underlines is
//! unrepresentable rather than merely unlikely.
//!
//! # What is not public
//!
//! A [`rowan::SyntaxNode`] is `!Send` and is a cursor into thread-local red-tree
//! state. ADR-095 keeps the option of moving the query layer onto its own thread
//! cheap by never letting one escape: [`Snapshot::parse`] is crate-private, and
//! every public answer is owned data or a [`TextRange`].

use std::cell::{Cell, OnceCell};
use std::collections::HashMap;
use std::rc::Rc;

use praxis_hir::{Analysis, ParserIndex, ResolvedRef};
use praxis_parser::ParseOutput;
use praxis_source::{diagnostic::sort_by_position, Diagnostic, FileId, LineMap, SourceMap};
use praxis_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use praxis_types::Type;
use rowan::{NodeOrToken, TextRange, TextSize};

use crate::document::{Document, Revision};
use crate::position::PositionMap;

/// One file at one revision, with the front end memoized on it.
///
/// The snapshot owns its own [`SourceMap`] holding exactly one file. That is
/// deliberate: `SourceMap::intern` is append-only by design ("the LSP will hold
/// several revisions of one file simultaneously"), so a process-global map in a
/// long-lived server would grow by one file per keystroke and never shrink. A
/// map per snapshot is bounded by the snapshot's own lifetime.
pub struct Snapshot {
    source: SourceMap,
    file: FileId,
    revision: Revision,
    text: String,
    lines: LineMap,
    parsed: OnceCell<ParseOutput>,
    analysis: OnceCell<Analysis>,
    /// How many times the parse actually ran. Memoization that is asserted
    /// rather than assumed — WS2's gate reads this.
    parse_runs: Cell<u32>,
    /// How many times inference actually ran, for the same reason.
    analyze_runs: Cell<u32>,
}

impl Snapshot {
    /// Build a snapshot for `text`, named `name` in diagnostics.
    #[must_use]
    pub fn new(name: &str, text: String, revision: Revision) -> Snapshot {
        let source = SourceMap::new();
        let file = source.intern(name, text.clone());
        let lines = LineMap::new(&text);
        Snapshot {
            source,
            file,
            revision,
            text,
            lines,
            parsed: OnceCell::new(),
            analysis: OnceCell::new(),
            parse_runs: Cell::new(0),
            analyze_runs: Cell::new(0),
        }
    }

    /// Build a snapshot for an open document.
    #[must_use]
    pub fn for_document(name: &str, doc: &Document) -> Snapshot {
        Snapshot::new(name, doc.text().to_string(), doc.revision())
    }

    /// §14.2 `source_text(file)`.
    #[must_use]
    pub fn source_text(&self) -> &str {
        &self.text
    }

    /// The source map this snapshot's diagnostics reference. `praxis check`
    /// renders through it.
    #[must_use]
    pub fn source_map(&self) -> &SourceMap {
        &self.source
    }

    #[must_use]
    pub fn file(&self) -> FileId {
        self.file
    }

    #[must_use]
    pub fn revision(&self) -> Revision {
        self.revision
    }

    #[must_use]
    pub fn positions(&self) -> PositionMap<'_> {
        PositionMap::new(&self.text, &self.lines)
    }

    #[must_use]
    pub fn line_map(&self) -> &LineMap {
        &self.lines
    }

    /// §14.2 `parse(file)`. **Crate-private** — see the module docs and
    /// ADR-095: handing a `SyntaxNode` across the crate boundary is what would
    /// make a later thread split a rewrite instead of a move.
    pub(crate) fn parse(&self) -> &ParseOutput {
        self.parsed.get_or_init(|| {
            self.parse_runs.set(self.parse_runs.get() + 1);
            praxis_parser::parse(self.file, &self.text)
        })
    }

    pub(crate) fn tree(&self) -> &SyntaxNode {
        &self.parse().tree
    }

    /// §14.2 `lower_ast`/`module_scope`/`infer_function` collapsed into the one
    /// call the front end actually exposes: `praxis_hir::analyze_root` runs name
    /// resolution and inference together and returns everything either produced.
    #[must_use]
    pub fn analyze(&self) -> &Analysis {
        self.analysis.get_or_init(|| {
            self.analyze_runs.set(self.analyze_runs.get() + 1);
            praxis_hir::analyze_root(self.file, self.tree())
        })
    }

    /// Every diagnostic for this file, in source order.
    ///
    /// **The one place the set and the order are decided** (ADR-097). Lex and
    /// parse diagnostics first by construction, then name and type diagnostics,
    /// all sorted by span — which is what `praxis check` printed from its own
    /// private copy of this sequence until M11 deleted it. The comparator itself
    /// is [`Diagnostic::sort_key`], shared with every other stage that merges two
    /// diagnostic lists; what is decided *here* is the set and the sequence.
    ///
    /// Analysis runs even when parsing reported: recovery keeps the tree usable,
    /// and a file with one stray token still deserves its type errors. That
    /// decision is stated here and nowhere else.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut all = self.parse().diagnostics.clone();
        all.extend(self.analyze().diagnostics.iter().cloned());
        sort_by_position(&mut all);
        all
    }

    /// §14.2 `type_of(expression)`: the inferred type of the innermost
    /// expression node covering `offset`.
    #[must_use]
    pub fn type_of(&self, offset: u32) -> Option<Type> {
        let analysis = self.analyze();
        let token = self.token_at(offset)?;
        // Walk outward from the token to the first ancestor inference recorded a
        // type for. `expr_types` is keyed by `NodeKey` — range **and** kind —
        // so a `PATH_EXPR` and the `Ident` inside it do not collide.
        token.parent_ancestors().find_map(|node| {
            analysis
                .expr_types
                .get(&praxis_hir::NodeKey::of(&node))
                .copied()
        })
    }

    /// §14.2 `resolve_name(position)`.
    #[must_use]
    pub fn resolve_name(&self, offset: u32) -> Option<(TextRange, ResolvedRef)> {
        let analysis = self.analyze();
        let token = self.token_at(offset)?;
        let range = token.text_range();
        analysis.refs.get(&range).map(|r| (range, *r))
    }

    /// §14.2 `input_parser_at(position)`: the retained parser index whose
    /// `read`/`parse` body covers `offset` (ADR-098).
    #[must_use]
    pub fn input_parser_at(&self, offset: u32) -> Option<&ParserIndex> {
        self.analyze()
            .parser_exprs
            .iter()
            .filter(|idx| idx.contains(offset))
            // Nested `read`s are not expressible, but the innermost is still the
            // right answer if they ever become so.
            .min_by_key(|idx| u32::from(idx.expr_range.len()))
    }

    /// §14.2 `completion_context(position)`.
    #[must_use]
    pub fn completion_context(&self, offset: u32) -> CompletionContext {
        crate::completion::context_at(self, offset)
    }

    /// How many times the parse actually ran. Memoization proved, not assumed.
    #[must_use]
    pub fn parse_runs(&self) -> u32 {
        self.parse_runs.get()
    }

    /// How many times inference actually ran.
    #[must_use]
    pub fn analyze_runs(&self) -> u32 {
        self.analyze_runs.get()
    }

    /// The token covering `offset`.
    ///
    /// A caret sits **between** characters, so an offset on a token boundary has
    /// two candidates and the choice is not arbitrary. The rule is
    /// *most-meaningful wins, ties to the right*:
    ///
    /// - a word — an identifier, keyword, literal or template — beats
    ///   punctuation, so hovering the `x` of `out(x)` answers about `x` and not
    ///   about the `(` that ends where it begins;
    /// - punctuation beats trivia, so completion just after `grid.` finds the
    ///   `.` and not the whitespace behind it;
    /// - equal rank goes to the token starting at the offset, which is where the
    ///   caret visually is.
    pub(crate) fn token_at(&self, offset: u32) -> Option<SyntaxToken> {
        let size = TextSize::from(offset.min(self.text.len() as u32));
        let tree = self.tree();
        match tree.token_at_offset(size) {
            rowan::TokenAtOffset::None => None,
            rowan::TokenAtOffset::Single(t) => Some(t),
            rowan::TokenAtOffset::Between(left, right) => {
                if rank(left.kind()) > rank(right.kind()) {
                    Some(left)
                } else {
                    Some(right)
                }
            }
        }
    }

    /// The nearest non-trivia token at or before `offset`.
    pub(crate) fn token_before(&self, offset: u32) -> Option<SyntaxToken> {
        let mut token = self.token_at(offset)?;
        if u32::from(token.text_range().start()) >= offset || token.kind().is_trivia() {
            loop {
                token = token.prev_token()?;
                if !token.kind().is_trivia() {
                    break;
                }
            }
        }
        Some(token)
    }

    /// The innermost node of `kind` containing `offset`, if any.
    pub(crate) fn ancestor_of_kind(&self, offset: u32, kind: SyntaxKind) -> Option<SyntaxNode> {
        self.token_at(offset)?
            .parent_ancestors()
            .find(|n| n.kind() == kind)
    }
}

/// What completion should offer at a position (§15.2's context list).
///
/// The receiver's *type* rather than its text: `grid.` is not an expression, so
/// there is no expression to re-infer — but inference already recorded the
/// receiver's type at the position it did parse, which is §8's measured result
/// and the reason completion needs no parser change.
#[derive(Clone, Debug)]
pub enum CompletionContext {
    /// After a `.`: offer this receiver type's fields and methods.
    Dot {
        receiver: Type,
        /// The partially-typed member name, if the user has started one.
        prefix: String,
    },
    /// Parser-expression mode — §7.5's constructors and §7.4's atomics.
    ///
    /// **A capture body is one of these** (D10): `{items:csv(int)}` is §7.7's
    /// own example, so the items offered inside `{…}` and outside it are the
    /// same set. `mode` records which, for the caller that wants to say so;
    /// it does not change the list.
    ///
    /// `enclosing` is the constructor whose argument list the cursor is in, if
    /// any — its named argument (`skip:`, `fill:`) and its `ragged` flag are
    /// offered alongside, read from [`Constructor`](praxis_input_parser::Constructor)
    /// and never from a name list.
    Parser {
        mode: praxis_hir::ParserMode,
        enclosing: Option<praxis_input_parser::Constructor>,
        prefix: String,
    },
    /// Inside a `Name { … }` record literal: offer the record's field names.
    RecordFields { record: Type, prefix: String },
    /// Inside a match pattern: offer the scrutinee enum's variant names.
    EnumCases { scrutinee: Type, prefix: String },
    /// Anywhere else: offer the lexical identifiers in scope.
    Lexical { prefix: String },
}

impl CompletionContext {
    /// The partially-typed word the user has entered, which every context
    /// filters by.
    #[must_use]
    pub fn prefix(&self) -> &str {
        match self {
            CompletionContext::Dot { prefix, .. }
            | CompletionContext::Parser { prefix, .. }
            | CompletionContext::RecordFields { prefix, .. }
            | CompletionContext::EnumCases { prefix, .. }
            | CompletionContext::Lexical { prefix } => prefix,
        }
    }
}

/// The snapshot cache: one live snapshot per URI, rebuilt when the revision
/// moves.
#[derive(Default)]
pub struct Analyzer {
    cache: HashMap<String, Rc<Snapshot>>,
}

impl Analyzer {
    #[must_use]
    pub fn new() -> Analyzer {
        Analyzer::default()
    }

    /// The snapshot for `doc`, reusing the cached one when the revision has not
    /// moved.
    ///
    /// Dropping the superseded `Rc` drops its `SourceMap`, its tree and its
    /// `Analysis` together — which is what keeps a server that has been open
    /// for an hour from holding an hour of keystrokes.
    pub fn snapshot(&mut self, key: &str, doc: &Document) -> Rc<Snapshot> {
        if let Some(existing) = self.cache.get(key) {
            if existing.revision() == doc.revision() {
                return Rc::clone(existing);
            }
        }
        let fresh = Rc::new(Snapshot::for_document(key, doc));
        self.cache.insert(key.to_string(), Rc::clone(&fresh));
        fresh
    }

    /// Forget a closed document.
    pub fn forget(&mut self, key: &str) {
        self.cache.remove(key);
    }
}

/// How much a token means to a cursor sitting on its boundary. See
/// [`Snapshot::token_at`].
fn rank(kind: SyntaxKind) -> u8 {
    if kind.is_trivia() {
        return 0;
    }
    if kind.is_keyword() {
        return 2;
    }
    match kind {
        SyntaxKind::Ident
        | SyntaxKind::IntLit
        | SyntaxKind::FloatLit
        | SyntaxKind::TextLit
        | SyntaxKind::CharLit
        | SyntaxKind::InterpOpen
        | SyntaxKind::InterpMiddle
        | SyntaxKind::InterpClose
        | SyntaxKind::BacktickTemplate
        | SyntaxKind::UnterminatedBacktickTemplate => 2,
        _ => 1,
    }
}

/// The expression node immediately to the left of `dot`, if there is one.
///
/// §8's measurement: for `grid.` the tree is `EXPR_STMT [ PATH_EXPR "grid", DOT ]`
/// — the postfix loop breaks and the checkpoint never becomes a node, but the
/// receiver is left as a **complete** `PATH_EXPR` right before the `DOT`. So
/// walking left from the dot finds a node inference has a type for, and no
/// parser recovery is needed.
pub(crate) fn expr_before_dot(dot: &SyntaxToken) -> Option<SyntaxNode> {
    let mut sibling = dot.prev_sibling_or_token();
    while let Some(element) = sibling {
        match element {
            NodeOrToken::Node(n) => return Some(n),
            NodeOrToken::Token(t) if t.kind().is_trivia() => sibling = t.prev_sibling_or_token(),
            NodeOrToken::Token(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn snap(text: &str) -> Snapshot {
        Snapshot::new("q.px", text.to_string(), Revision(0))
    }

    /// **WS2's gate.** Two queries at one revision parse once.
    #[test]
    fn two_queries_at_one_revision_parse_once() {
        let s = snap("var x = 1\nout(x)\n");
        let _ = s.diagnostics();
        let _ = s.type_of(4);
        let _ = s.analyze();
        assert_eq!(s.parse_runs(), 1, "the parse is memoized by revision");
        assert_eq!(s.analyze_runs(), 1, "and so is the analysis");
    }

    /// …and an edit invalidates it.
    #[test]
    fn an_edit_invalidates_the_snapshot() {
        let mut analyzer = Analyzer::new();
        let mut doc = Document::new("var x = 1\n".to_string(), 1);
        let first = analyzer.snapshot("k", &doc);
        let again = analyzer.snapshot("k", &doc);
        assert!(
            Rc::ptr_eq(&first, &again),
            "the same revision is the same snapshot"
        );

        doc.apply(
            &lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "var x = 2\n".to_string(),
            },
            crate::position::Encoding::Utf16,
        );
        let after = analyzer.snapshot("k", &doc);
        assert!(!Rc::ptr_eq(&first, &after), "an edit builds a new snapshot");
        assert_eq!(after.source_text(), "var x = 2\n");
    }

    #[test]
    fn diagnostics_are_sorted_by_span() {
        let s = snap("var a: Int = \"x\"\nvar b: Text = 1\n");
        let diags = s.diagnostics();
        assert!(diags.len() >= 2, "two mismatches, got {}", diags.len());
        let mut starts: Vec<u32> = diags
            .iter()
            .map(|d| d.primary().span.start().to_u32())
            .collect();
        let sorted = {
            let mut c = starts.clone();
            c.sort_unstable();
            c
        };
        assert_eq!(starts, sorted);
        starts.dedup();
    }

    /// A file whose parse reported still gets its analysis: recovery keeps the
    /// tree usable, and the editor must not go blank on one stray character.
    #[test]
    fn analysis_runs_even_when_parsing_reported() {
        let s = snap("var x = 1\n@\nvar y: Int = \"t\"\n");
        let diags = s.diagnostics();
        assert!(
            diags.iter().any(|d| d.code().to_string().starts_with('T')),
            "a lex diagnostic"
        );
        assert!(
            diags.iter().any(|d| d.code().to_string().starts_with('Y')),
            "and a type diagnostic from the same file: {:?}",
            diags
                .iter()
                .map(|d| d.code().to_string())
                .collect::<Vec<_>>()
        );
    }

    /// **ADR-133, at the surface the user reported it from.** The editor
    /// publishes exactly this list, so a diagnostic missing here is a program the
    /// compiler refuses and the editor calls fine.
    ///
    /// Each of these was reported by `praxis run` and by nothing else, because it
    /// was raised while the program was being lowered and this query does not
    /// lower. The first two are the user's own report.
    #[test]
    fn the_editor_sees_every_diagnostic_run_refuses_a_program_for() {
        for (src, want) in [
            (
                "enum Bla { A(Int), B, C }\nvar bla = A(3)\nmatch bla { A(i, j) => {} B => {} C => {} }\n",
                "Y124",
            ),
            (
                "enum Bla { A(Int), B, C }\nvar bla = A(3)\nmatch bla { A => {} B => {} C => {} }\n",
                "Y124",
            ),
            ("var x = 99999999999999999999999\nout(x)\n", "Y013"),
            (
                "struct Point { x: Int, y: Int }\nvar pts = [Point{x: 0, y: 1}]\n\
                 for Point { x: 0, y } in pts {\n    out(y)\n}\n",
                "Y125",
            ),
        ] {
            let s = snap(src);
            let diags = s.diagnostics();
            assert!(
                diags.iter().any(|d| d.code().to_string() == want),
                "{src}\nmust publish {want}, got {:?}",
                diags
                    .iter()
                    .map(|d| d.code().to_string())
                    .collect::<Vec<_>>()
            );
        }
    }

    /// `type_of` answers the **innermost** expression covering the offset, which
    /// is not always the one a reader has in mind: on the callee name it is the
    /// callee, and on the call it is the call.
    #[test]
    fn type_of_answers_the_innermost_expression() {
        let src = "var v = Vec[Int]()\n";
        let s = snap(src);
        let db = &s.analyze().db;

        let on_callee = u32::try_from(src.find("Vec").unwrap()).unwrap();
        let callee = s.type_of(on_callee).expect("the callee has a type");
        assert_eq!(
            db.render(db.follow(callee)),
            "() -> Vec[Int]",
            "on the name, the innermost expression is the callee itself"
        );

        let on_call = u32::try_from(src.find("()").unwrap() + 1).unwrap();
        let call = s.type_of(on_call).expect("the call has a type");
        assert_eq!(db.render(db.follow(call)), "Vec[Int]");
    }

    #[test]
    fn input_parser_at_finds_the_read_body() {
        let src = "var v = read lines(int)\n";
        let s = snap(src);
        let at = u32::try_from(src.find("lines").unwrap()).unwrap();
        assert!(s.input_parser_at(at).is_some());
        assert!(s.input_parser_at(0).is_none(), "`var` is outside the body");
    }
}
