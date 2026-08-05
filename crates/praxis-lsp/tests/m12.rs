//! §19.12's deliverables and acceptance criteria, as gates over the query
//! layer.
//!
//! Each assertion names the thing an implementation could get *plausibly* wrong
//! and still pass a weaker test: references matching a word rather than a
//! symbol; rename accepting a collision because nothing in it looked; an inlay
//! hint next to an annotation the author already wrote; a code action offered
//! whose edit does not compile; documentation the language server wrote itself
//! rather than reading the compiler's table.

use std::path::PathBuf;
use std::str::FromStr;

use lsp_types::{
    CodeActionOrCommand, Hover, HoverContents, InlayHintLabel, MarkupContent, OneOf, Position,
    PrepareRenameResponse, Range, Uri,
};
use praxis_lsp::position::Encoding;
use praxis_lsp::query::Snapshot;
use praxis_lsp::{DocumentStore, Revision};

const ENC: Encoding = Encoding::Utf8;

fn snap(text: &str) -> Snapshot {
    Snapshot::new("gate.px", text.to_string(), Revision(0))
}

fn uri() -> Uri {
    Uri::from_str("file:///gate.px").expect("a valid URI")
}

/// The offset of `needle`'s first occurrence.
fn at(src: &str, needle: &str) -> u32 {
    u32::try_from(
        src.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is in the fixture")),
    )
    .expect("fixtures are small")
}

/// The offset of the occurrence of `needle` that follows `after`.
fn at_after(src: &str, after: &str, needle: &str) -> u32 {
    let base = src.find(after).expect("the anchor is in the fixture") + after.len();
    u32::try_from(base + src[base..].find(needle).expect("`needle` follows")).expect("small")
}

fn whole_file() -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: u32::MAX,
            character: 0,
        },
    }
}

fn hover_text(h: &Hover) -> String {
    match &h.contents {
        HoverContents::Markup(MarkupContent { value, .. }) => value.clone(),
        other => panic!("hover must be Markdown, got {other:?}"),
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("m12-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create this process's scratch directory");
    dir
}

// ---------------------------------------------------------------------------
// Find references
// ---------------------------------------------------------------------------

/// **References follow the symbol, not the word.** Two bindings share the
/// spelling `a`; the second one's uses are its own, and the initializer that
/// reads the *first* is not among them.
///
/// A text search would return four ranges for either query.
#[test]
fn references_are_a_symbols_and_not_a_spellings() {
    let src = "var a = 1\nvar a = a + 1\nout(a)\n";
    let s = snap(src);

    let second_decl = at_after(src, "var a = 1\nvar ", "a");
    let found = praxis_lsp::navigation::references(&s, second_decl, &uri(), ENC, true)
        .expect("the declaration resolves");
    let lines: Vec<u32> = found.iter().map(|l| l.range.start.line).collect();
    assert_eq!(
        lines,
        vec![1, 2],
        "the second `a`: its own declaration and the `out(a)` that reads it"
    );

    let first_decl = at(src, "a = 1");
    let first = praxis_lsp::navigation::references(&s, first_decl, &uri(), ENC, true)
        .expect("the first declaration resolves");
    let first_lines: Vec<u32> = first.iter().map(|l| l.range.start.line).collect();
    assert_eq!(
        first_lines,
        vec![0, 1],
        "the first `a`: its declaration and the initializer on line 2 that reads it"
    );
}

/// The client's `includeDeclaration` is honoured rather than ignored.
#[test]
fn the_declaration_is_included_only_when_asked_for() {
    let src = "var total = 0\nout(total)\n";
    let s = snap(src);
    let offset = at(src, "total");
    let with = praxis_lsp::navigation::references(&s, offset, &uri(), ENC, true).expect("resolves");
    let without =
        praxis_lsp::navigation::references(&s, offset, &uri(), ENC, false).expect("resolves");
    assert_eq!(with.len(), 2);
    assert_eq!(without.len(), 1);
    assert_eq!(without[0].range.start.line, 1, "the use, not the `var`");
}

// ---------------------------------------------------------------------------
// Rename — §19.12 criterion 1
// ---------------------------------------------------------------------------

/// **"Rename updates all valid references."** Every range the symbol owns, and
/// no range belonging to the binding it shadows.
#[test]
fn rename_edits_every_reference_to_the_symbol_and_no_other() {
    let src = "var a = 1\nvar a = a + 1\nout(a)\n";
    let s = snap(src);
    let edit =
        praxis_lsp::rename::rename(&s, at_after(src, "var a = 1\nvar ", "a"), "b", &uri(), ENC)
            .expect("a fresh name is safe");
    let edits = edit
        .changes
        .expect("edits are per-URI")
        .remove(&uri())
        .expect("this file");
    assert_eq!(edits.len(), 2, "{edits:?}");
    assert!(
        edits.iter().all(|e| e.new_text == "b"),
        "every edit writes the new name"
    );
    assert!(
        edits.iter().all(|e| e.range.start.line != 0),
        "the shadowed binding on line 1 is untouched: {edits:?}"
    );
}

/// **"…and rejects unsafe collisions."** Three shapes of unsafe, which is the
/// point: the check is not a list of collision kinds, it is the resolver's own
/// answer to whether anything changed meaning.
#[test]
fn rename_rejects_the_three_shapes_of_collision() {
    // 1. The new name is a prelude binding, and a reference would be captured.
    let src = "var n = 1\nout(n)\n";
    let s = snap(src);
    let refused = praxis_lsp::rename::rename(&s, at(src, "n"), "out", &uri(), ENC)
        .expect_err("`out` is already what the call resolves to");
    assert!(
        refused.to_string().contains("`out`"),
        "the message names the collision: {refused}"
    );

    // 2. A second declaration of a name that already exists at top level.
    let two = "fn f() -> Int { 1 }\nfn g() -> Int { 2 }\nout(f() + g())\n";
    let s = snap(two);
    assert!(
        praxis_lsp::rename::rename(&s, at(two, "g"), "f", &uri(), ENC).is_err(),
        "renaming `g` to `f` collides with the `fn f` beside it"
    );
    // …and the same rename to a free name is accepted, so the refusal above is
    // about the collision and not about `fn` names being unrenameable.
    assert!(praxis_lsp::rename::rename(&s, at(two, "g"), "h", &uri(), ENC).is_ok());

    // 3. A capture: an outer binding a reference would start resolving to.
    let capture = "var outer = 1\nvar inner = 2\nout(inner + outer)\n";
    let s = snap(capture);
    assert!(
        praxis_lsp::rename::rename(&s, at(capture, "inner"), "outer", &uri(), ENC).is_err(),
        "`inner` renamed to `outer` changes what `outer` on the last line means"
    );
}

/// A spelling the lexer would not read back is refused before anything is
/// analyzed, and the keyword list is the lexer's own.
#[test]
fn rename_refuses_a_spelling_that_is_not_a_name() {
    let src = "var n = 1\nout(n)\n";
    let s = snap(src);
    for bad in ["var", "fn", "match", "1x", "", "a b"] {
        assert!(
            praxis_lsp::rename::rename(&s, at(src, "n"), bad, &uri(), ENC).is_err(),
            "`{bad}` is not a usable name"
        );
    }
}

/// `prepareRename` refuses a prelude name **before** the user types a
/// replacement — it is declared in the compiler, not in this file.
#[test]
fn prepare_rename_refuses_a_name_this_file_does_not_declare() {
    let src = "var n = 1\nout(n)\n";
    let s = snap(src);
    assert!(
        praxis_lsp::rename::prepare(&s, at(src, "out"), ENC).is_none(),
        "`out` has no declaration site to edit"
    );
    let ready = praxis_lsp::rename::prepare(&s, at(src, "n"), ENC).expect("a local can be renamed");
    match ready {
        PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => {
            assert_eq!(placeholder, "n");
        }
        other => panic!("expected a range and a placeholder, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Workspace symbols
// ---------------------------------------------------------------------------

/// Symbols come from every `.px` file under the root, and an **open buffer wins
/// over the bytes on disk** — the picker offers what the user is looking at.
#[test]
fn workspace_symbols_span_the_folder_and_prefer_the_open_buffer() {
    let dir = scratch_dir("symbols");
    std::fs::write(dir.join("day01.px"), "fn parse_day_one() -> Int { 1 }\n").expect("write");
    std::fs::write(dir.join("day02.px"), "struct Point { x: Int, y: Int }\n").expect("write");
    // Skipped: not a `.px` file, and a build directory.
    std::fs::write(dir.join("notes.txt"), "fn not_a_symbol() {}\n").expect("write");
    std::fs::create_dir_all(dir.join("target")).expect("mkdir");
    std::fs::write(
        dir.join("target/day03.px"),
        "fn built_output() -> Int { 3 }\n",
    )
    .expect("write");

    let roots = vec![dir.clone()];
    let mut docs = DocumentStore::new();
    let all = praxis_lsp::workspace::symbols("", &roots, &docs, ENC);
    let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"parse_day_one"), "{names:?}");
    assert!(names.contains(&"Point"), "{names:?}");
    assert!(
        names.contains(&"x"),
        "a struct's fields are symbols too: {names:?}"
    );
    assert!(!names.contains(&"not_a_symbol"), "only `.px`: {names:?}");
    assert!(
        !names.contains(&"built_output"),
        "`target/` is skipped: {names:?}"
    );

    // The query filters, case-insensitively.
    let filtered = praxis_lsp::workspace::symbols("POIN", &roots, &docs, ENC);
    assert_eq!(
        filtered.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["Point"]
    );

    // Now open `day01.px` with different, unsaved text.
    let day01 = praxis_lsp::workspace::path_to_uri(&dir.join("day01.px")).expect("absolute");
    docs.open(
        day01,
        "fn renamed_in_the_buffer() -> Int { 1 }\n".to_string(),
        1,
    );
    let after = praxis_lsp::workspace::symbols("", &roots, &docs, ENC);
    let names: Vec<&str> = after.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"renamed_in_the_buffer"), "{names:?}");
    assert!(
        !names.contains(&"parse_day_one"),
        "the buffer replaced the file's own text: {names:?}"
    );

    // A symbol's location is its own name, so a picker jumps to the
    // declaration rather than the top of the file.
    let point = after.iter().find(|s| s.name == "Point").expect("Point");
    match &point.location {
        OneOf::Left(location) => assert_eq!(location.range.start.character, 7),
        OneOf::Right(_) => {
            panic!("a location without a range needs client support we do not ask for")
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Inlay hints
// ---------------------------------------------------------------------------

/// **The milestone's headline.** `fn foo(a, b)` reads as `fn foo(a: Int, b: Int)`
/// in the editor, and a parameter inference could not pin shows `?T` rather than
/// nothing.
#[test]
fn hints_write_the_inferred_type_beside_a_parameter() {
    let src = "fn foo(a, b) { a + b }\nfn id(t) { t }\nout(foo(1, 2))\n";
    let s = snap(src);
    let hints = praxis_lsp::inlay::hints(&s, whole_file(), ENC);
    let labels: Vec<(u32, u32, String)> = hints
        .iter()
        .map(|h| (h.position.line, h.position.character, label_of(h)))
        .collect();
    assert_eq!(
        labels,
        vec![
            (0, 8, ": Int".to_string()),
            (0, 11, ": Int".to_string()),
            (1, 7, ": ?T".to_string()),
        ],
        "one hint per unannotated binding, at the end of its name"
    );
}

/// A binding the author annotated gets **no** hint — including a closure
/// parameter, whose name lives under a `PATTERN` node and so is invisible to a
/// search of direct children.
#[test]
fn an_annotated_binding_gets_no_hint() {
    let src = "var typed: Int = 1\nvar f = |q: Int| q + 1\nfn g(p: Int) -> Int { p }\nout(f(typed) + g(1))\n";
    let s = snap(src);
    let hints = praxis_lsp::inlay::hints(&s, whole_file(), ENC);
    let labels: Vec<String> = hints.iter().map(label_of).collect();
    assert_eq!(
        labels,
        vec![": (Int) -> Int".to_string()],
        "only `f`, which has no annotation of its own: {labels:?}"
    );
}

/// A hint carries the edit that writes the annotation — but only where the
/// annotation is legal *and* the type is one the parser reads back.
#[test]
fn a_hint_offers_an_edit_only_where_the_annotation_would_compile() {
    let src = "fn foo(a) { a + 1 }\nfn id(t) { t }\nvar v = read lines(`{x:int}`)\nvar ns = Vec[Int]()\nfor k in ns { out(k) }\nout(foo(1) + v.len())\n";
    let s = snap(src);
    let hints = praxis_lsp::inlay::hints(&s, whole_file(), ENC);
    let by_label: Vec<(String, bool)> = hints
        .iter()
        .map(|h| (label_of(h), h.text_edits.is_some()))
        .collect();
    assert!(
        by_label.contains(&(": Int".to_string(), true)),
        "a `fn` parameter's `Int` is spellable: {by_label:?}"
    );
    assert!(
        by_label.contains(&(": ?T".to_string(), false)),
        "`?T` names a variable nothing binds, so there is no edit: {by_label:?}"
    );
    assert!(
        by_label
            .iter()
            .any(|(l, edit)| l.contains("{ x: Int }") && !edit),
        "an anonymous record has no annotation syntax: {by_label:?}"
    );
    assert!(
        by_label.iter().any(|(l, edit)| l == ": Int" && !edit),
        "a `for` variable has nowhere to write an annotation: {by_label:?}"
    );
}

/// Applying a hint's edit produces a file that still checks clean — the only
/// test that catches an annotation the grammar would refuse.
#[test]
fn applying_a_hints_edit_keeps_the_file_clean() {
    let src = "fn foo(a, b) { a + b }\nvar v = Vec[Int]()\nout(foo(1, 2) + v.len())\n";
    let s = snap(src);
    let mut edited = src.to_string();
    let mut edits: Vec<(usize, String)> = Vec::new();
    for hint in praxis_lsp::inlay::hints(&s, whole_file(), ENC) {
        let Some(text_edits) = hint.text_edits else {
            continue;
        };
        for edit in text_edits {
            let offset = s.positions().offset(edit.range.start, ENC) as usize;
            edits.push((offset, edit.new_text));
        }
    }
    edits.sort_by_key(|(offset, _)| std::cmp::Reverse(*offset));
    for (offset, text) in edits {
        edited.insert_str(offset, &text);
    }
    let after = snap(&edited);
    assert!(
        after.diagnostics().is_empty(),
        "the annotated program must be clean.\n--- {edited}--- got {:?}",
        after
            .diagnostics()
            .iter()
            .map(|d| format!("{} {}", d.code(), d.message()))
            .collect::<Vec<_>>()
    );
    assert!(edited.contains("fn foo(a: Int, b: Int)"), "{edited}");
}

/// A `read` bound straight to a `var` is hinted **once**: the binding's hint
/// already names the type, and a second one beside it would say it twice.
#[test]
fn a_parser_root_is_hinted_only_where_the_binding_does_not_say_it() {
    let bound = snap("var v = read lines(int)\nout(v.len())\n");
    assert_eq!(
        praxis_lsp::inlay::hints(&bound, whole_file(), ENC).len(),
        1,
        "the `var`'s hint, and nothing else"
    );

    let unbound = snap("out(parse(\"1\", int))\n");
    let labels: Vec<String> = praxis_lsp::inlay::hints(&unbound, whole_file(), ENC)
        .iter()
        .map(label_of)
        .collect();
    assert_eq!(
        labels,
        vec![": Int".to_string()],
        "a `parse` with no binding shows its own result type"
    );
}

/// The client's visible range is honoured.
#[test]
fn hints_outside_the_requested_range_are_not_returned() {
    let src = "fn foo(a) { a + 1 }\nfn bar(b) { b + 1 }\nout(foo(1) + bar(2))\n";
    let s = snap(src);
    let first_line_only = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 100,
        },
    };
    let hints = praxis_lsp::inlay::hints(&s, first_line_only, ENC);
    assert_eq!(hints.len(), 1, "{hints:?}");
    assert_eq!(hints[0].position.line, 0);
}

fn label_of(hint: &lsp_types::InlayHint) -> String {
    match &hint.label {
        InlayHintLabel::String(s) => s.clone(),
        other => panic!("hints are plain strings, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Code actions — §19.12 criterion 3
// ---------------------------------------------------------------------------

/// **"Code actions can fix misspelled parser constructors."** §15.3's own
/// example: `line` is offered `lines`, and applying the edit produces a file
/// that checks clean.
#[test]
fn a_misspelled_parser_constructor_has_a_fix_that_compiles() {
    let src = "var v = read line(int)\nout(v.len())\n";
    assert_fix_makes_it_clean(src, "line", "did you mean `lines`");
}

/// …and the same inside a template capture, where the name is scanned rather
/// than parsed.
#[test]
fn a_misspelled_capture_parser_has_a_fix_too() {
    let src = "var v = read lines(`{n:it}`)\nout(v.len())\n";
    assert_fix_makes_it_clean(src, "it}", "did you mean `int`");
}

/// **"…and add missing match arms."**
#[test]
fn a_non_exhaustive_match_has_a_fix_that_compiles() {
    let src = "enum E { A, B }\nfn f(e: E) -> Int {\n    match e {\n        A => 1\n    }\n}\nout(f(A))\n";
    assert_fix_makes_it_clean(src, "match e", "add the missing match arms");
}

/// A misspelled binding and a misspelled method are the same mistake and get
/// the same treatment.
#[test]
fn a_misspelled_name_and_method_have_fixes() {
    assert_fix_makes_it_clean("var total = 1\nout(totl)\n", "totl", "did you mean `total`");
    assert_fix_makes_it_clean(
        "var v = Vec[Int]()\nout(v.lenn())\n",
        "lenn",
        "did you mean `len`",
    );
}

/// A fix is offered only where the cursor is. A code action list for line 1 of a
/// file whose mistake is on line 9 is a menu of edits to somewhere else.
#[test]
fn actions_are_scoped_to_the_requested_range() {
    let src = "var total = 1\nout(totl)\n";
    let s = snap(src);
    let elsewhere = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 3,
        },
    };
    assert!(
        praxis_lsp::code_action::actions(&s, elsewhere, &uri(), ENC).is_empty(),
        "line 1 has no mistake"
    );
    assert!(!praxis_lsp::code_action::actions(&s, whole_file(), &uri(), ENC).is_empty());
}

/// Apply the one quick fix offered at `needle` and require the result to be a
/// clean program. The title is checked too, so a test cannot pass on some
/// *other* fix that happened to be offered at the same place.
fn assert_fix_makes_it_clean(src: &str, needle: &str, expect_title: &str) {
    let s = snap(src);
    let offset = at(src, needle);
    let position = s.positions().position(offset, ENC);
    let range = Range {
        start: position,
        end: position,
    };
    let actions = praxis_lsp::code_action::actions(&s, range, &uri(), ENC);
    assert_eq!(
        actions.len(),
        1,
        "exactly one fix at `{needle}` in\n{src}\ngot {actions:?}"
    );
    let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("a quick fix is a CodeAction, not a Command");
    };
    assert!(
        action.title.to_lowercase().contains(expect_title),
        "expected a fix titled like `{expect_title}`, got `{}`",
        action.title
    );
    assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));

    let edits = action
        .edit
        .clone()
        .expect("a quick fix carries an edit")
        .changes
        .expect("edits are per-URI")
        .remove(&uri())
        .expect("this file");
    let mut edited = src.to_string();
    let mut spans: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|e| {
            let span = s.positions().span(e.range, ENC);
            (
                span.start().to_u32() as usize,
                span.end().to_u32() as usize,
                e.new_text.clone(),
            )
        })
        .collect();
    spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, text) in spans {
        edited.replace_range(start..end, &text);
    }

    let after = snap(&edited);
    assert!(
        after.diagnostics().is_empty(),
        "applying the fix must produce a clean file.\n--- fixed ---\n{edited}--- got ---\n{:?}",
        after
            .diagnostics()
            .iter()
            .map(|d| format!("{} {}", d.code(), d.message()))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Documentation in hover
// ---------------------------------------------------------------------------

/// A method's hover carries the catalog's own signature and documentation.
#[test]
fn hovering_a_method_shows_its_signature_and_documentation() {
    let src = "var v = Vec[Int]()\nout(v.len())\n";
    let s = snap(src);
    let text = hover_text(
        &praxis_lsp::hover::hover(&s, at(src, "len"), ENC).expect("a method call hovers"),
    );
    assert!(text.contains("Vec[Int].len("), "the signature: {text}");
    assert!(text.contains("-> Int"), "the result type: {text}");
    // The catalog's own sentence, not one written in the language server.
    let catalog = praxis_stdlib::builtin_catalog();
    let entry = catalog
        .entries()
        .iter()
        .find(|e| e.name == "len")
        .expect("`len` is in the catalog");
    assert!(
        text.contains(entry.doc),
        "the documentation is the catalog's: {text}"
    );
}

/// A parser constructor's hover carries §7.5's signature and description, and
/// still says what the expression's type is.
#[test]
fn hovering_a_parser_constructor_shows_its_documentation() {
    let src = "var v = read lines(int)\nout(v.len())\n";
    let s = snap(src);
    let text =
        hover_text(&praxis_lsp::hover::hover(&s, at(src, "lines"), ENC).expect("hover answers"));
    assert!(text.contains("lines(parser) -> Vec[T]"), "{text}");
    assert!(
        text.contains(praxis_input_parser::Constructor::Lines.doc()),
        "the documentation is the constructor table's: {text}"
    );
    assert!(text.contains("Vec[Int]"), "still names the type: {text}");

    // An atomic gets its own row, and its result type.
    let atom = hover_text(
        &praxis_lsp::hover::hover(&s, at(src, "int)"), ENC).expect("hover answers on the atomic"),
    );
    assert!(atom.contains("int -> Int"), "{atom}");
    assert!(
        atom.contains(praxis_input_parser::AtomicKind::Int.doc()),
        "{atom}"
    );
}
