//! ADR-098's gate: the parser AST survives inference, with a type per node.
//!
//! The type assertions are stated against *inner* nodes — not "hover returned
//! something", but "hover on the inner constructor returned the inner
//! constructor's type". An implementation that answers the root's type for
//! every position inside the expression passes any weaker test and fails
//! these.

#![cfg(test)]

use praxis_source::Span;

use crate::hir_tests::test_util::analyze;
use crate::ParserMode;

/// The byte offset of the first occurrence of `needle` in `src`.
fn at(src: &str, needle: &str) -> u32 {
    u32::try_from(
        src.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` in source")),
    )
    .unwrap()
}

/// The source text `span` covers. Every span in this module is a claim about
/// what the compiler pointed at, so the assertion is against the *text* rather
/// than against offsets nobody can read.
fn text_at(src: &str, span: Span) -> &str {
    &src[span.start().to_u32() as usize..span.end().to_u32() as usize]
}

/// **ADR-098's gate.** Hovering `lines(...)` *inside* `sections(lines(...))`
/// answers that node's type, not the root's.
#[test]
fn an_inner_constructor_has_its_own_synthesized_type() {
    let src = "var v = read sections(lines(`{a:int},{b:int}`))\n";
    let analysis = analyze(src);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    assert_eq!(analysis.parser_exprs.len(), 1, "one `read` body");
    let index = &analysis.parser_exprs[0];

    let root = index
        .type_at(at(src, "sections"))
        .expect("the root has a type");
    let inner = index
        .type_at(at(src, "lines"))
        .expect("the inner constructor has a type");

    assert_eq!(
        analysis.db.render(analysis.db.follow(root)),
        "Vec[Vec[{ a: Int, b: Int }]]"
    );
    assert_eq!(
        analysis.db.render(analysis.db.follow(inner)),
        "Vec[{ a: Int, b: Int }]",
        "the inner node's type, not the root's"
    );
}

/// A capture's own parser is a node too: `{a:int}`'s `int` is `Int`, and the
/// template around it is the record.
#[test]
fn a_capture_type_and_its_template_have_distinct_types() {
    let src = "var v = read lines(`{a:int},{b:int}`)\n";
    let analysis = analyze(src);
    let index = &analysis.parser_exprs[0];

    // The `int` of `{a:int}` — the first one, so `find` is unambiguous.
    let int_at = at(src, "int}");
    let capture_ty = index
        .type_at(int_at)
        .expect("the capture parser has a type");
    assert_eq!(analysis.db.render(analysis.db.follow(capture_ty)), "Int");

    let template_at = at(src, "`{a");
    let template_ty = index.type_at(template_at).expect("the template has a type");
    assert_eq!(
        analysis.db.render(analysis.db.follow(template_ty)),
        "{ a: Int, b: Int }"
    );
}

/// §15.3's five-way question, answered from the spans the compiler computed.
#[test]
fn the_cursor_mode_narrows_from_expression_to_atomic_name() {
    let src = "var v = read lines(`x {n:int}`)\n";
    let analysis = analyze(src);
    let index = &analysis.parser_exprs[0];

    // Before the parser expression starts.
    assert_eq!(index.mode_at(at(src, "var")), ParserMode::Outside);
    // On the constructor name.
    assert_eq!(index.mode_at(at(src, "lines")), ParserMode::Expression);
    // On the literal `x` inside the template.
    assert_eq!(index.mode_at(at(src, "x {")), ParserMode::Template);
    // On the capture's name.
    assert_eq!(index.mode_at(at(src, "n:int")), ParserMode::Capture);
    // On the capture's parser.
    assert_eq!(index.mode_at(at(src, "int}")), ParserMode::AtomicName);
}

/// The four spans §19.11 criterion 4 needs are four **distinct** ranges, and
/// each is the compiler's own — the capture name is the trimmed name and not the
/// braces around it.
#[test]
fn a_capture_reports_its_name_and_parser_spans_separately() {
    let src = "var v = read lines(`{name:word} {n:int}`)\n";
    let analysis = analyze(src);
    let index = &analysis.parser_exprs[0];

    let captures = index.captures();
    assert_eq!(captures.len(), 2, "two captures");

    let first = captures[0];
    assert_eq!(
        text_at(src, first.span),
        "{name:word}",
        "the capture span covers both braces"
    );
    let name_span = first.name_span.expect("a named capture has a name span");
    assert_eq!(text_at(src, name_span), "name");
    assert_eq!(text_at(src, first.parser_span), "word");
    assert_ne!(
        name_span, first.parser_span,
        "four distinct ranges, not two"
    );
}

/// An anonymous capture has no name span. `{int}` names nothing, and reporting
/// an empty span at the brace would put a "capture name" token on a `{`.
#[test]
fn an_anonymous_capture_has_no_name_span() {
    let src = "var v = read lines(`{int}`)\n";
    let analysis = analyze(src);
    let captures = analysis.parser_exprs[0].captures();
    assert_eq!(captures.len(), 1);
    assert!(captures[0].name_span.is_none());
}

/// A capture name written with spaces around it — `{ n :int}` — names `n`, and
/// the span covers the name alone rather than the padding around it.
#[test]
fn a_padded_capture_name_spans_only_the_name() {
    let src = "var v = read lines(`{ n :int}`)\n";
    let analysis = analyze(src);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    let captures = analysis.parser_exprs[0].captures();
    let name_span = captures[0].name_span.expect("named");
    assert_eq!(
        text_at(src, name_span),
        "n",
        "the span is the trimmed name, not the padding around it"
    );
}

/// The literal runs carry the source they were decoded from, and the decoded
/// text is not the same length as that source when an escape contributed.
#[test]
fn a_template_literal_run_spans_the_source_it_was_decoded_from() {
    let src = "var v = read lines(`a\\`b{n:int}`)\n";
    let analysis = analyze(src);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    let lits: Vec<_> = analysis.parser_exprs[0]
        .template_literals()
        .into_iter()
        .filter(|s| s.end().to_u32() > s.start().to_u32())
        .collect();
    assert_eq!(lits.len(), 1, "one literal run, got {lits:?}");
    let s = text_at(src, lits[0]);
    assert_eq!(s, "a\\`b", "four source bytes for three decoded characters");
}

/// The constructor name span is the keyword and nothing else — `lines`, not
/// `lines(...)` — and it comes from `Constructor::keyword`, the same closed
/// table the node was built from.
#[test]
fn a_constructor_reports_its_keyword_span() {
    let src = "var v = read sections(lines(int))\n";
    let analysis = analyze(src);
    let ctors = analysis.parser_exprs[0].constructors();
    let named: Vec<(&str, &str)> = ctors
        .iter()
        .map(|(span, kw)| (text_at(src, *span), *kw))
        .collect();
    assert_eq!(named, vec![("sections", "sections"), ("lines", "lines")]);
}

/// A `read` whose body the compiler rejected records **no** index entry. An
/// entry for a tree that failed to convert would answer hover about a program
/// that does not exist.
#[test]
fn a_rejected_parser_expression_records_no_index_entry() {
    let src = "var v = read frobnicate(int)\n";
    let analysis = analyze(src);
    assert!(
        !analysis.diagnostics.is_empty(),
        "an unknown constructor reports"
    );
    assert!(analysis.parser_exprs.is_empty(), "and indexes nothing");
}

/// `parse(text, P)` is indexed on the same footing as `read P` — §7.1 enters the
/// sublanguage at both words.
#[test]
fn a_parse_expression_is_indexed_too() {
    let src = "fn f(s: Text) -> Vec[Int] { parse(s, lines(int)) }\n";
    let analysis = analyze(src);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    assert_eq!(analysis.parser_exprs.len(), 1);
    let ty = analysis.parser_exprs[0]
        .type_at(at(src, "lines"))
        .expect("indexed");
    assert_eq!(analysis.db.render(analysis.db.follow(ty)), "Vec[Int]");
}

/// Two `read`s in one file are two index entries, each covering its own extent.
#[test]
fn two_read_expressions_are_two_entries() {
    let src = "fn a() -> Vec[Int] { read lines(int) }\nfn b() -> Grid[Char] { read grid(char) }\n";
    let analysis = analyze(src);
    assert_eq!(analysis.parser_exprs.len(), 2);
    let first = &analysis.parser_exprs[0];
    let second = &analysis.parser_exprs[1];
    assert!(first.contains(at(src, "lines")));
    assert!(!first.contains(at(src, "grid")));
    assert!(second.contains(at(src, "grid")));
}
