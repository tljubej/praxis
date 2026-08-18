//! Hover, completion, signature help, definition and semantic tokens, as
//! gates over the query layer.
//!
//! Each assertion here names the thing an implementation could get *plausibly*
//! wrong and still pass a weaker test: hover on an inner constructor answering
//! the root's type; completion returning "some methods"; signature help always
//! reporting parameter 0; definition landing on a name match rather than a
//! symbol; semantic tokens "being produced". Those are the assertions.

use lsp_types::{
    CompletionItemKind, DiagnosticSeverity, Hover, HoverContents, MarkupContent, NumberOrString,
    Uri,
};
use praxis_lsp::position::Encoding;
use praxis_lsp::query::Snapshot;
use praxis_lsp::{Revision, Server};
use std::str::FromStr;

fn snap(text: &str) -> Snapshot {
    Snapshot::new("gate.px", text.to_string(), Revision(0))
}

fn at(src: &str, needle: &str) -> u32 {
    u32::try_from(
        src.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is in the fixture")),
    )
    .expect("fixtures are small")
}

fn hover_text(h: &Hover) -> String {
    match &h.contents {
        HoverContents::Markup(MarkupContent { value, .. }) => value.clone(),
        other => panic!("hover must be Markdown, got {other:?}"),
    }
}

fn uri() -> Uri {
    Uri::from_str("file:///gate.px").expect("a valid URI")
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// **An edit introduces a `Y0xx`, and the fix retracts it.**
///
/// The code *and* the span are asserted, not "something was published": a
/// server that published one diagnostic with the wrong code at offset zero
/// would satisfy a count.
#[test]
fn an_edit_publishes_a_code_and_a_span_and_the_fix_retracts_it() {
    let mut server = Server::new(Encoding::Utf16);
    server.open(uri(), "var x: Int = 1\n".to_string(), 1);

    let clean = server.diagnostics_for(&uri()).expect("an open document");
    assert!(
        clean.diagnostics.is_empty(),
        "the file starts clean: {:?}",
        clean.diagnostics
    );

    // Break it: `Int = "t"`.
    let text = "var x: Int = \"t\"\n";
    server.documents_mut().open(uri(), text.to_string(), 2);
    let broken = server.diagnostics_for(&uri()).expect("an open document");
    assert_eq!(broken.diagnostics.len(), 1, "{:?}", broken.diagnostics);
    let d = &broken.diagnostics[0];
    assert_eq!(
        d.code,
        Some(NumberOrString::String("Y001".to_string())),
        "the registered code (ADR-051 owns the allocation)"
    );
    assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(d.range.start.line, 0);
    assert_eq!(d.range.start.character, 13, "the span is the `\"t\"`");
    assert_eq!(d.range.end.character, 16);

    // Fix it.
    server
        .documents_mut()
        .open(uri(), "var x: Int = 1\n".to_string(), 3);
    let fixed = server.diagnostics_for(&uri()).expect("an open document");
    assert!(
        fixed.diagnostics.is_empty(),
        "the fix retracts it, got {:?}",
        fixed.diagnostics
    );
}

/// A note becomes `relatedInformation` rather than being flattened into the
/// message, so an editor can jump to the second span.
#[test]
fn a_diagnostic_carries_its_notes_as_related_information() {
    let mut server = Server::new(Encoding::Utf16);
    // A mismatch whose wording carries a note is the exhaustiveness family; a
    // plain mismatch may not have one, so this asserts the *mapping* holds
    // whenever notes exist rather than that a particular program produces one.
    server.open(
        uri(),
        "enum E { A, B }\nfn f(e: E) -> Int { match e { E::A => 1 } }\n".to_string(),
        1,
    );
    let published = server.diagnostics_for(&uri()).expect("open");
    for d in &published.diagnostics {
        if let Some(related) = &d.related_information {
            assert!(
                related.iter().all(|r| !r.message.is_empty()),
                "a related span must say why it is related"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------------

/// **Criterion 3, twice.** Hovering `read`'s body shows the synthesized result
/// type, and hovering `lines(...)` *inside* it shows **that node's** type.
///
/// The second half is what ADR-098's spanned index exists for: with only the
/// root's type in reach, an implementation that answered it at every offset
/// inside the body would still pass the first half.
#[test]
fn hover_shows_the_read_result_and_the_inner_constructors_own_type() {
    let src = "var v = read sections(lines(`{a:int},{b:int}`))\n";
    let s = snap(src);

    let root = praxis_lsp::hover::hover(&s, at(src, "sections"), Encoding::Utf16)
        .expect("hovering the root answers");
    let root_text = hover_text(&root);
    assert!(
        root_text.contains("Vec[Vec[{ a: Int, b: Int }]]"),
        "the `read` result type, got {root_text}"
    );

    let inner = praxis_lsp::hover::hover(&s, at(src, "lines"), Encoding::Utf16)
        .expect("hovering the inner constructor answers");
    let inner_text = hover_text(&inner);
    assert!(
        inner_text.contains("Vec[{ a: Int, b: Int }]"),
        "the inner node's own type, got {inner_text}"
    );
    assert!(
        !inner_text.contains("Vec[Vec["),
        "and **not** the root's, got {inner_text}"
    );
}

/// §15.2's own hover example shape: a segments-style binding renders the
/// anonymous record with its fields.
#[test]
fn hover_renders_an_anonymous_record_with_its_fields() {
    let src = "var segments = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)\n";
    let s = snap(src);
    assert!(
        s.diagnostics().is_empty(),
        "{:?}",
        s.diagnostics()
            .iter()
            .map(|d| d.message().to_string())
            .collect::<Vec<_>>()
    );
    let h = praxis_lsp::hover::hover(&s, at(src, "segments"), Encoding::Utf16)
        .expect("hovering the binding answers");
    let text = hover_text(&h);
    for field in ["x1: Int", "y1: Int", "x2: Int", "y2: Int"] {
        assert!(text.contains(field), "missing `{field}` in {text}");
    }
}

/// Hovering a capture's parser answers the capture's type, not the template's.
#[test]
fn hover_on_a_capture_type_answers_that_capture() {
    let src = "var v = read lines(`{name:word} {n:int}`)\n";
    let s = snap(src);
    let h = praxis_lsp::hover::hover(&s, at(src, "word"), Encoding::Utf16).expect("answers");
    assert!(hover_text(&h).contains("Text"), "{}", hover_text(&h));
    let h = praxis_lsp::hover::hover(&s, at(src, "int}"), Encoding::Utf16).expect("answers");
    assert!(hover_text(&h).contains("Int"), "{}", hover_text(&h));
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

/// **Criterion 2 exactly.** `grid.` offers the grid methods, each with its
/// parameter and result types — **and only** the grid methods.
///
/// The absence assertion is the one that matters: a `Map` method appearing in
/// the list is what "offer every catalog entry" looks like, and it passes any
/// test that only checks a grid method is present.
#[test]
fn typing_grid_dot_offers_grid_methods_with_signatures_and_nothing_else() {
    let src = "fn main() -> Unit {\n  var grid = read grid(char)\n  grid.\n}\n";
    let s = snap(src);
    let cursor = at(src, "grid.\n") + 5;
    let ctx = s.completion_context(cursor);
    let items = praxis_lsp::completion::items(&s, &ctx);
    assert!(!items.is_empty(), "`grid.` must offer something");

    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    for grid_method in ["width", "height", "neighbors4", "row", "column"] {
        assert!(
            labels.contains(&grid_method),
            "`{grid_method}` is a Grid method and must be offered; got {labels:?}"
        );
    }

    // `insert` is `Map[K, V]`'s and `push` is `Vec[T]`'s. Neither is a Grid's.
    for foreign in ["insert", "push", "pop"] {
        assert!(
            !labels.contains(&foreign),
            "`{foreign}` is not a Grid method and must be absent; got {labels:?}"
        );
    }

    // "with signatures": every method carries its parameter and result types,
    // from `completion_data`'s `params`/`result` and not from a second table.
    let width = items
        .iter()
        .find(|i| i.label == "width")
        .expect("`width` is offered");
    assert_eq!(width.kind, Some(CompletionItemKind::METHOD));
    let detail = width.detail.as_deref().expect("a signature");
    assert!(
        detail.starts_with('(') && detail.contains("->"),
        "the detail is a signature, got {detail}"
    );
    assert!(
        width.documentation.is_some(),
        "the catalog's doc rides along"
    );

    // The catalog's **operator** rows are not names: `grid.[]` is not syntax.
    for operator in ["[]", "[]=", "[]min=", "[]max="] {
        assert!(
            !labels.contains(&operator),
            "`{operator}` is a subscript operator, not a member name; got {labels:?}"
        );
    }
}

/// **ADR-127 decision 1, at the editor.** `set.` offers the whole pipeline, and
/// `grid.` still does not.
///
/// The rule dispatch applies and the rule completion applies are the *same
/// function*, `praxis_stdlib::pattern_matches`. Two copies of it could disagree
/// and let the editor offer a method the compiler refuses; sharing the function
/// is what makes that unrepresentable, and this is what checks that the generic
/// receiver reached both sides.
#[test]
fn typing_set_dot_offers_the_pipeline_and_a_grid_still_does_not() {
    let src = "fn main() -> Unit {\n  var s = Set()\n  s.insert(1)\n  s.\n}\n";
    let s = snap(src);
    let labels = labels_at(&s, at(src, "s.\n}") + 2);

    // The twenty-three fused rows, the eight barriers and the eight
    // conversions all reach a `Set`. A spread of them, including the ones that
    // give a `Set` what the language does not: it has no member accessor at
    // all, so `to_vec` is how a member is read out of one.
    //
    // `join` is offered on a `Set[Int]` on purpose. `pattern_matches`'s
    // `Iterable` arm looks at the receiver's *shape* and not at its item, so the
    // row matches and the item bound is reported by the compiler at the call —
    // `expected Text, found Int` — rather than by the completion list going
    // quiet. That is the documented split, and offering only what would
    // type-check would mean re-deriving unification inside the LSP.
    for offered in [
        "map",
        "filter",
        "sum",
        "count",
        "fold",
        "any",
        "find",
        "sorted",
        "sorted_by_key",
        "unique",
        "reversed",
        "frequencies",
        "join",
        "to_vec",
        "to_map",
        "to_counter",
        "to_bitset",
    ] {
        assert!(
            labels.contains(&offered.to_string()),
            "`{offered}` is a pipeline row and a `Set` is one of the ten; got {labels:?}"
        );
    }
    // …and the `Set`'s own rows are still there beside them.
    for own in ["insert", "remove", "contains", "len"] {
        assert!(labels.contains(&own.to_string()), "{own}: {labels:?}");
    }
    // A `Map` method is still a `Map`'s.
    for foreign in ["keys", "values", "push"] {
        assert!(
            !labels.contains(&foreign.to_string()),
            "`{foreign}` is not a `Set` method; got {labels:?}"
        );
    }

    // **The rule still holds**, and it holds for the reason
    // the decision gives rather than by accident: `Grid` is out of
    // `PIPELINE_RECEIVERS`, so no pipeline row matches it.
    let grid_src = "fn main() -> Unit {\n  var grid = read grid(char)\n  grid.\n}\n";
    let s = snap(grid_src);
    let labels = labels_at(&s, at(grid_src, "grid.\n") + 5);
    for absent in [
        "map",
        "filter",
        "sum",
        "to_set",
        "sorted_by_key",
        "reversed",
        "join",
    ] {
        assert!(
            !labels.contains(&absent.to_string()),
            "`grid.{absent}` would claim §6.4's shape-preserving name; got {labels:?}"
        );
    }
    assert!(labels.contains(&"cells".to_string()), "{labels:?}");

    // A `Vec` offers `to_text`, which no `Iterable` row carries: it is a
    // concrete `Vec[T where Is(Char)]` row (ADR-144), and `pattern_matches`
    // treats a bounded element as a wildcard, so the bound is reported at the
    // call rather than by the row failing to match.
    let vec_src = "fn main() -> Unit {\n  var v = [1, 2]\n  v.\n}\n";
    let s = snap(vec_src);
    let labels = labels_at(&s, at(vec_src, "v.\n}") + 2);
    assert!(labels.contains(&"to_text".to_string()), "{labels:?}");
    assert!(labels.contains(&"reversed".to_string()), "{labels:?}");
}

/// Completion in parser-expression mode offers §7.4's atomics and §7.5's
/// constructors — from the closed tables, so a name added later is offered
/// without anybody editing this crate.
#[test]
fn completion_inside_a_read_offers_the_atomics_and_constructors() {
    let src = "var v = read lines(int)\n";
    let s = snap(src);
    let ctx = s.completion_context(at(src, "int)"));
    let items = praxis_lsp::completion::items(&s, &ctx);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    for atomic in praxis_input_parser::AtomicKind::ALL {
        assert!(
            labels.contains(&atomic.keyword()),
            "`{}` is a §7.4 atomic and must be offered; got {labels:?}",
            atomic.keyword()
        );
    }
    for ctor in praxis_input_parser::Constructor::ALL {
        assert!(
            labels.contains(&ctor.keyword()),
            "`{}` is a §7.5 constructor and must be offered",
            ctor.keyword()
        );
    }
}

/// A parser constructor's **named argument** comes from the constructor, never
/// from a name list: `chars` offers `skip:` and `grid` offers `fill:` and
/// `ragged`, and neither offers the other's.
#[test]
fn a_parser_named_argument_is_read_from_its_constructor() {
    let chars_src = "var v = read chars(char, )\n";
    let s = snap(chars_src);
    let labels = labels_at(&s, at(chars_src, ", )") + 2);
    assert!(labels.contains(&"skip:".to_string()), "{labels:?}");
    assert!(
        !labels.contains(&"fill:".to_string()),
        "`fill:` is `grid`'s, not `chars`'s: {labels:?}"
    );

    let grid_src = "var v = read grid(char, )\n";
    let s = snap(grid_src);
    let labels = labels_at(&s, at(grid_src, ", )") + 2);
    assert!(labels.contains(&"fill:".to_string()), "{labels:?}");
    assert!(labels.contains(&"ragged".to_string()), "{labels:?}");
    assert!(
        !labels.contains(&"skip:".to_string()),
        "`skip:` is `chars`'s, not `grid`'s: {labels:?}"
    );
}

fn labels_at(s: &Snapshot, offset: u32) -> Vec<String> {
    let ctx = s.completion_context(offset);
    praxis_lsp::completion::items(s, &ctx)
        .into_iter()
        .map(|i| i.label)
        .collect()
}

/// §15.2's "enum cases in patterns": inside a match **pattern**, the scrutinee's
/// variants are offered. The arm's *body* is an ordinary expression and gets the
/// lexical list instead — which is the distinction worth asserting, because a
/// context test that only looks at the enclosing `match` cannot make it.
#[test]
fn completion_in_a_match_pattern_offers_the_scrutinees_variants() {
    let src =
        "enum Dir { North, South }\nfn f(d: Dir) -> Int { match d { North => 1, South => 2 } }\n";
    let s = snap(src);

    let in_pattern = labels_at(&s, at(src, "North => 1"));
    assert!(in_pattern.contains(&"North".to_string()), "{in_pattern:?}");
    assert!(in_pattern.contains(&"South".to_string()), "{in_pattern:?}");

    let in_body = labels_at(&s, at(src, "1, South"));
    assert!(
        in_body.contains(&"d".to_string()) || in_body.contains(&"f".to_string()),
        "the arm body takes the lexical list, got {in_body:?}"
    );
}

/// The lexical fallback offers the names in the file and the prelude, and
/// filters by what has been typed.
#[test]
fn lexical_completion_offers_declared_names_and_filters_by_prefix() {
    let src = "fn helper() -> Int { 1 }\nfn main() -> Int { var total = 1\n  he\n}\n";
    let s = snap(src);
    let labels = labels_at(&s, at(src, "he\n") + 2);
    assert!(
        labels.contains(&"helper".to_string()),
        "a declared fn is offered, got {labels:?}"
    );
    assert!(
        labels.iter().all(|l| l.starts_with("he")),
        "the prefix filters the list, got {labels:?}"
    );
}

/// A trigger character earns the menu only where it means what it was
/// registered for.
///
/// The three template characters exist for the parser sublanguage; outside one
/// `{` opens every block and `:` introduces every type annotation, and the
/// context resolved there is `Lexical` with an empty prefix — every name in the
/// file, offered over a name the user has not finished inventing. The gate is
/// per-character rather than per-context on purpose: `{` keeps `Name { … }`,
/// which is the one non-template place a brace introduces a closed list.
#[test]
fn a_template_trigger_character_is_silent_outside_a_template() {
    use praxis_lsp::completion::trigger_answers_here;

    let block = "fn helper() -> Int { 1 }\nfn main() -> Unit {\n  var t = 1\n}\n";
    let s = snap(block);
    let brace = s.completion_context(at(block, "Unit {") + 6);
    assert!(
        !trigger_answers_here("{", &brace),
        "a block's `{{` must not open a menu, context was {brace:?}"
    );
    // …and the list behind it is exactly the one that must not appear.
    assert!(
        labels_at(&s, at(block, "Unit {") + 6).contains(&"out".to_string()),
        "the gate, not an empty list, is what keeps this out of the editor"
    );

    let annotation = "fn f(n: Int) -> Int { n }\n";
    let s = snap(annotation);
    let colon = s.completion_context(at(annotation, ": Int") + 1);
    assert!(
        !trigger_answers_here(":", &colon),
        "a type annotation's `:` must not open a menu, context was {colon:?}"
    );

    let record = "struct P { x: Int, y: Int }\nfn main() -> Unit { var p = P {} }\n";
    let s = snap(record);
    let lit = s.completion_context(at(record, "P {}") + 3);
    assert!(
        trigger_answers_here("{", &lit),
        "a record literal's `{{` still answers, context was {lit:?}"
    );

    let template = "var v = read `{n:int}`\n";
    let s = snap(template);
    for (trigger, offset) in [("{", 1), (":", 3)] {
        let ctx = s.completion_context(at(template, "{n:int}") + offset);
        assert!(
            trigger_answers_here(trigger, &ctx),
            "`{trigger}` is why these are registered at all, context was {ctx:?}"
        );
    }

    let dot = "fn main() -> Unit { var v = [1].reversed() }\n";
    let s = snap(dot);
    assert!(
        trigger_answers_here(".", &s.completion_context(at(dot, ".reversed") + 1)),
        "`.` is unambiguous and is not gated"
    );
    assert!(
        !trigger_answers_here(".", &s.completion_context(at(dot, "var v") + 4)),
        "…but only where it actually is a member access"
    );
}

/// A record literal's field names come from the record's own definition.
#[test]
fn completion_in_a_record_literal_offers_its_fields() {
    let src = "struct P { x: Int, y: Int }\nfn main() -> Unit { var p = P { x: 1, y: 2 } }\n";
    let s = snap(src);
    let labels = labels_at(&s, at(src, "x: 1"));
    assert!(labels.contains(&"x".to_string()), "{labels:?}");
    assert!(labels.contains(&"y".to_string()), "{labels:?}");
}

// ---------------------------------------------------------------------------
// Signature help
// ---------------------------------------------------------------------------

/// **The active-parameter index advances across a comma**, asserted per
/// position. An implementation that always answers `0` passes any "signature
/// help was returned" test.
#[test]
fn the_active_parameter_advances_across_a_comma() {
    let src = "fn add(a: Int, b: Int) -> Int { a + b }\nfn main() -> Int { add(1, 2) }\n";
    let s = snap(src);
    let call = at(src, "add(1, 2)");

    let on_first = praxis_lsp::signature::signature_help(&s, call + 4).expect("in the arg list");
    assert_eq!(on_first.active_parameter, Some(0), "before the comma");

    let on_second = praxis_lsp::signature::signature_help(&s, call + 7).expect("in the arg list");
    assert_eq!(on_second.active_parameter, Some(1), "after the comma");

    let sig = &on_second.signatures[0];
    assert!(sig.label.starts_with("add:"), "{}", sig.label);
    let params = sig.parameters.as_ref().expect("two parameters");
    assert_eq!(params.len(), 2);
}

/// A parser constructor's signature is derived from §7.5's own argument-shape
/// table, and `sections` has the two forms §15.2 spells out.
#[test]
fn signature_help_on_a_parser_constructor_offers_both_sections_forms() {
    let src = "var v = read sections(lines(int))\n";
    let s = snap(src);
    let help = praxis_lsp::signature::signature_help(&s, at(src, "lines(int)"))
        .expect("in the constructor's arg list");
    let labels: Vec<&str> = help.signatures.iter().map(|s| s.label.as_str()).collect();
    assert!(labels.contains(&"sections(parser) -> Vec[T]"), "{labels:?}");
    assert!(
        labels.contains(&"sections(name: parser, ..., tail: repeated(parser)) -> record"),
        "{labels:?}"
    );
}

/// A method call's signature comes from the catalog entry dispatch selected, so
/// the receiver and result are the concrete ones at this site.
#[test]
fn signature_help_on_a_method_names_the_receiver_and_result() {
    let src = "fn main(v: Vec[Int]) -> Unit { v.push(1) }\n";
    let s = snap(src);
    let help = praxis_lsp::signature::signature_help(&s, at(src, "push(1)") + 5)
        .expect("in the method's arg list");
    let label = &help.signatures[0].label;
    assert!(label.starts_with("Vec[Int].push("), "{label}");
    assert!(label.contains("->"), "{label}");
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

/// **Definition from the second of two shadowed bindings lands on the second
/// declaration.**
///
/// The one assertion that distinguishes a real symbol table from a name match:
/// both declarations spell `a`, and only distinct `SymbolId`s tell them apart.
#[test]
fn definition_from_a_shadowed_use_lands_on_the_second_declaration() {
    let src = "var a = 4\nvar a = \"Foo\"\nout(a)\n";
    let s = snap(src);
    let use_site = at(src, "out(a)") + 4;
    let location = praxis_lsp::navigation::goto_definition(&s, use_site, &uri(), Encoding::Utf16)
        .expect("the use resolves");

    // The second `a` is on line 1 (0-based), at character 4.
    assert_eq!(
        (location.range.start.line, location.range.start.character),
        (1, 4),
        "the **second** declaration, not the first"
    );
}

/// Document symbols cover the top-level declaration forms, with a struct's
/// fields and an enum's variants nested under them.
#[test]
fn document_symbols_list_the_top_level_declarations() {
    let src = "struct P { x: Int }\nenum E { A, B }\nfn f() -> Int { 1 }\nvar top = 1\n";
    let s = snap(src);
    let symbols = praxis_lsp::navigation::document_symbols(&s, Encoding::Utf16);
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["P", "E", "f", "top"]);

    let p = &symbols[0];
    let fields: Vec<&str> = p
        .children
        .as_ref()
        .expect("a struct lists its fields")
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(fields, vec!["x"]);

    let e = &symbols[1];
    let variants: Vec<&str> = e
        .children
        .as_ref()
        .expect("an enum lists its variants")
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(variants, vec!["A", "B"]);
}

// ---------------------------------------------------------------------------
// Semantic tokens
// ---------------------------------------------------------------------------

/// **Criterion 4.** Over `` read lines(`{name:word} {n:int}`) `` the four
/// parser classes land on four **distinct ranges** with four **distinct token
/// types** — asserted as (text, type) pairs, not as "tokens were produced".
#[test]
fn the_four_parser_classes_land_on_four_distinct_ranges() {
    let src = "var v = read lines(`{name:word} {n:int}`)\n";
    let s = snap(src);
    assert!(s.diagnostics().is_empty(), "the fixture is clean");

    let tokens = praxis_lsp::semantic::classify(&s);
    let named: Vec<(&str, &str)> = tokens
        .iter()
        .map(|t| {
            (
                &src[t.span.start().to_u32() as usize..t.span.end().to_u32() as usize],
                t.ty.name(),
            )
        })
        .collect();

    for expected in [
        ("lines", "parserConstructor"),
        ("name", "parserCaptureName"),
        ("word", "parserCaptureType"),
        ("n", "parserCaptureName"),
        ("int", "parserCaptureType"),
        (" ", "parserTemplateText"),
    ] {
        assert!(
            named.contains(&expected),
            "missing {expected:?}; got {named:?}"
        );
    }

    // Four distinct types over four distinct ranges.
    let parser_tokens: Vec<_> = tokens
        .iter()
        .filter(|t| t.ty.name().starts_with("parser"))
        .collect();
    let types: std::collections::HashSet<&str> =
        parser_tokens.iter().map(|t| t.ty.name()).collect();
    assert_eq!(
        types.len(),
        4,
        "four distinct parser classes, got {types:?}"
    );
    let ranges: std::collections::HashSet<(u32, u32)> = parser_tokens
        .iter()
        .map(|t| (t.span.start().to_u32(), t.span.end().to_u32()))
        .collect();
    assert_eq!(
        ranges.len(),
        parser_tokens.len(),
        "every parser token has its own range"
    );
}

/// Tokens are sorted and disjoint, which the protocol requires and which a
/// client renders unpredictably without.
#[test]
fn semantic_tokens_are_sorted_and_disjoint() {
    let src = "struct P { x: Int }\nfn main() -> Unit {\n  var v: Vec[Int] = Vec[Int]()\n  \
               var g = read grid(char)\n  var s = read lines(`{a:int},{b:int}`)\n  out(v.len())\n}\n";
    let s = snap(src);
    let tokens = praxis_lsp::semantic::classify(&s);
    assert!(!tokens.is_empty());
    for pair in tokens.windows(2) {
        assert!(
            pair[0].span.end().to_u32() <= pair[1].span.start().to_u32(),
            "tokens overlap: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
}

/// The editor can tell a `Grid[T]` from a local named `grid` — which is the
/// whole reason semantic tokens exist beside a regex grammar.
#[test]
fn a_local_named_grid_is_a_variable_and_not_a_constructor() {
    let src = "fn main() -> Unit {\n  var grid = read grid(char)\n  out(grid)\n}\n";
    let s = snap(src);
    let tokens = praxis_lsp::semantic::classify(&s);
    let find = |offset: u32| {
        tokens
            .iter()
            .find(|t| t.span.start().to_u32() == offset)
            .map(|t| t.ty.name())
    };
    assert_eq!(
        find(at(src, "grid = read")),
        Some("variable"),
        "the binding is a local"
    );
    assert_eq!(
        find(at(src, "grid(char)")),
        Some("parserConstructor"),
        "the constructor inside the `read` body is not"
    );
}

/// The encoded form is deltas, and the first token's delta is its absolute
/// position. Asserted once, so the classification tests can stay in absolute
/// spans.
#[test]
fn the_encoded_tokens_are_relative_to_their_predecessor() {
    let src = "var x = 1\nvar y = 2\n";
    let s = snap(src);
    let encoded = praxis_lsp::semantic::tokens(&s, Encoding::Utf16);
    assert!(encoded.data.len() >= 4, "{:?}", encoded.data);
    assert_eq!(
        encoded.data[0].delta_line, 0,
        "the first token is on line 0"
    );
    assert_eq!(encoded.data[0].delta_start, 0, "`var` starts the file");
    // The `var` on line 1 is one line down from the `1` on line 0.
    let second_line = encoded
        .data
        .iter()
        .find(|t| t.delta_line == 1)
        .expect("a token on the next line");
    assert_eq!(second_line.delta_start, 0, "and at its start");
}

/// The two iteration surfaces answer in the editor the same way they do at
/// `praxis check` — which they do by construction (ADR-097), so what this pins
/// is that each has a *type recorded on its own node*.
///
/// The list literal's is the one an implementation gets plausibly wrong: a
/// `LIST_EXPR` holds its elements inside an `ARG_LIST`, so a hover that walked
/// to the nearest typed ancestor would answer `Int` on the whole literal — the
/// element's type, at the literal's offset.
#[test]
fn a_list_literal_and_a_texts_loop_variable_hover_as_themselves() {
    let src = "fn main() -> Unit {\n  var xs = [1, 2, 3]\n  for c in \"ab\" { out(c) }\n}\n";
    let s = snap(src);
    assert!(
        s.diagnostics().is_empty(),
        "{:?}",
        s.diagnostics()
            .iter()
            .map(|d| d.message().to_string())
            .collect::<Vec<_>>()
    );

    // The binding takes the literal's type.
    let h = praxis_lsp::hover::hover(&s, at(src, "xs"), Encoding::Utf16).expect("answers");
    let text = hover_text(&h);
    assert!(text.contains("Vec[Int]"), "the literal's type, got {text}");

    // …and the literal itself answers `Vec[Int]`, not its element's `Int`.
    let h = praxis_lsp::hover::hover(&s, at(src, "[1, 2, 3]"), Encoding::Utf16).expect("answers");
    let text = hover_text(&h);
    assert!(
        text.contains("Vec[Int]"),
        "the literal's own type, got {text}"
    );

    // A `Text`'s loop variable is a `Char`, which is what makes `c.to_int()`
    // complete rather than `c.len()`.
    let h = praxis_lsp::hover::hover(&s, at(src, "c in"), Encoding::Utf16).expect("answers");
    let text = hover_text(&h);
    assert!(text.contains("Char"), "the item type, got {text}");
    assert!(!text.contains("Text"), "and not the receiver's, got {text}");
}

/// A list literal is a `Vec`, so `[1, 2, 3].` offers the `Vec` methods — the
/// completion half of "a literal is a spelling, not a shape".
#[test]
fn a_list_literal_offers_vec_methods() {
    let src = "fn main() -> Unit {\n  [1, 2, 3].\n}\n";
    let s = snap(src);
    let cursor = at(src, "[1, 2, 3].") + 10;
    let ctx = s.completion_context(cursor);
    let items = praxis_lsp::completion::items(&s, &ctx);
    let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(names.contains(&"push"), "a Vec method, got {names:?}");
    assert!(names.contains(&"len"), "a Vec method, got {names:?}");
    // …and not another collection's, which is what "offer every catalog entry"
    // looks like.
    assert!(
        !names.contains(&"insert"),
        "a Set/Map method, got {names:?}"
    );
}

/// **ADR-147, editor side.** Inside an interpolated literal the *fragments* are
/// string and the *hole* is code.
///
/// This is the dividend of the hole being a real subtree rather than a substring
/// of one opaque token: the name in it is classified by what it resolves to, so
/// it is coloured, hoverable and renamable like any other reference. An
/// implementation that kept the literal whole would classify the whole thing
/// `string` and there would be nothing here to assert.
#[test]
fn a_hole_is_code_and_the_fragments_around_it_are_string() {
    let src = "fn main() -> Unit {\n  var total = 3\n  out(\"n = {total}!\")\n}\n";
    let s = snap(src);
    assert!(s.diagnostics().is_empty(), "{:?}", s.diagnostics());
    let tokens = praxis_lsp::semantic::classify(&s);
    let find = |offset: u32| {
        tokens
            .iter()
            .find(|t| t.span.start().to_u32() == offset)
            .map(|t| t.ty.name())
    };
    assert_eq!(
        find(at(src, "\"n = {")),
        Some("string"),
        "the opening fragment is a string"
    );
    assert_eq!(
        find(at(src, "total}!")),
        Some("variable"),
        "the name in the hole is the local it resolves to, not string"
    );
    assert_eq!(
        find(at(src, "}!\"")),
        Some("string"),
        "the closing fragment is a string"
    );
}

/// **ADR-147, the rest of the editor surface.** A hole is a real subtree, so
/// every query that is keyed on a resolved name's *range* answers inside one
/// without being taught about interpolation at all.
///
/// That is the claim worth pinning, because it is the one an implementation
/// could plausibly get wrong and still pass the semantic-token test above: a
/// design that re-lexed holes out of one opaque `TextLit` would paint them
/// correctly and still have nothing for hover, definition, references or rename
/// to attach to. Each assertion below names a query and the answer it must give
/// for the name `count`, which appears once as a declaration and twice in holes.
#[test]
fn every_name_query_answers_inside_a_hole() {
    let src = "var count = 3\nvar label = \"n={count} again={count}\"\nout(label)\n";
    let s = snap(src);
    // The two occurrences inside holes, found past the declaration.
    let first_hole = at(src, "{count}") + 1;
    let second_hole = at(src, "again={count}") + u32::try_from("again={".len()).unwrap();

    // Hover answers the binding's type, not the enclosing literal's `Text`.
    let h = praxis_lsp::hover::hover(&s, first_hole, Encoding::Utf16)
        .expect("hover answers inside a hole");
    assert!(
        hover_text(&h).contains("Int"),
        "hover in a hole answers the name's own type, got {:?}",
        hover_text(&h)
    );

    // Go-to-definition lands on the `var`, not on the literal.
    let target = praxis_lsp::navigation::goto_definition(&s, first_hole, &uri(), Encoding::Utf16)
        .expect("a hole's name has a definition");
    assert_eq!(
        target.range.start.line, 0,
        "the definition is the `var count` on line 0, got {target:?}"
    );

    // The name in a hole resolves to the *same symbol* as the declaration, which
    // is what makes references and rename whole-file rather than per-occurrence.
    let sym = praxis_lsp::navigation::symbol_at(&s, first_hole).expect("a hole's name is a symbol");
    let ranges = praxis_lsp::navigation::reference_ranges(&s, sym);
    assert_eq!(
        ranges.len(),
        3,
        "the declaration and both holes are references to one symbol, got {ranges:?}"
    );

    // Rename rewrites both holes. A rename that missed them would silently
    // change the program's meaning while leaving it compiling.
    let edit = praxis_lsp::rename::rename(&s, first_hole, "total", &uri(), Encoding::Utf16)
        .expect("a name in a hole is renameable");
    let edits = edit
        .changes
        .as_ref()
        .and_then(|c| c.get(&uri()))
        .expect("edits for this file");
    assert_eq!(
        edits.len(),
        3,
        "rename touches the declaration and both holes, got {edits:?}"
    );

    // And the second hole is reached independently, so this is not one lucky
    // offset: `symbol_at` answers there too.
    assert_eq!(
        praxis_lsp::navigation::symbol_at(&s, second_hole),
        Some(sym),
        "both holes name the same binding"
    );
}
