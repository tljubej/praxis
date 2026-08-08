//! WS10's drift gates: the TextMate grammar is checked against the compiler's
//! own closed tables.
//!
//! A grammar's keyword list is a **copy** of the lexer's, and no compiler checks
//! it. Every keyword added after M11 is a chance for the two to diverge
//! silently: the failure is a word quietly ceasing to be coloured, which nobody
//! files and no test would otherwise catch.
//!
//! So the grammar is read **at test time** rather than quoted here — the
//! `design_doc.rs` precedent applied to a second file: a test that quotes a file
//! can drift from it, a test that reads it cannot. All three source lists
//! (`SyntaxKind`'s keywords, `AtomicKind::ALL`, `Constructor::ALL`) are
//! `ALL`-swept closed tables, so each gate is a loop over one of them.
//!
//! **This adds no Node toolchain to CI.** `just ci` is the whole gate (ADR-002)
//! and a second toolchain in it is a cost this milestone does not need to pay.

use std::path::PathBuf;

mod common;

use common::bin_path;

/// The VS Code extension's directory.
fn editor_dir() -> PathBuf {
    common::workspace_root().join("editors/vscode")
}

fn read(relative: &str) -> String {
    let path = editor_dir().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("the extension ships `{}`: {e}", path.display()))
}

fn grammar() -> serde_json::Value {
    serde_json::from_str(&read("syntaxes/praxis.tmLanguage.json"))
        .expect("the grammar is well-formed JSON")
}

fn manifest() -> serde_json::Value {
    serde_json::from_str(&read("package.json")).expect("the manifest is well-formed JSON")
}

/// The `match`/`begin` regex of a named rule in the grammar's repository.
fn rule_pattern(grammar: &serde_json::Value, rule: &str) -> String {
    let entry = &grammar["repository"][rule];
    assert!(
        !entry.is_null(),
        "the grammar has no `{rule}` rule; the gate below cannot check what is not there"
    );
    entry["match"]
        .as_str()
        .or_else(|| entry["begin"].as_str())
        .unwrap_or_else(|| panic!("`{rule}` has neither a `match` nor a `begin`"))
        .to_string()
}

/// The whole alternatives of the first `(a|b|c)` group in `pattern`.
///
/// Exact rather than a substring search, and that is the point: `int` must not
/// be satisfied by the `int` inside `identifier`, and `word` must not be
/// satisfied by a rule that happens to contain the letters. A gate that can be
/// satisfied by an accident is not a gate — `the_alternation_reader_rejects_a_word_that_is_only_a_substring`
/// below is the proof that this one cannot.
fn alternatives(pattern: &str) -> Vec<String> {
    let start = match pattern.find('(') {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    let end = match pattern[start..].find(')') {
        Some(i) => start + i,
        None => return Vec::new(),
    };
    pattern[start..end]
        .split('|')
        .map(|alt| alt.trim().trim_start_matches("?:").to_string())
        .collect()
}

/// Whether `pattern`'s alternation offers `word` as a whole alternative.
fn offers(pattern: &str, word: &str) -> bool {
    alternatives(pattern).iter().any(|alt| alt == word)
}

/// The alternation reader is exact: a word that is only a **substring** of an
/// alternative is not offered.
///
/// Without this, `offers` could be satisfied by an accident of spelling — `int`
/// by the `int` inside `identifier`, `word` by any rule containing the letters —
/// and the four gates below would pass while the grammar was missing entries.
#[test]
fn the_alternation_reader_rejects_a_word_that_is_only_a_substring() {
    let pattern = r"\b(int|uint|identifier|word)\b";
    assert!(offers(pattern, "int"));
    assert!(offers(pattern, "identifier"));
    assert!(!offers(pattern, "ident"), "a prefix is not an alternative");
    assert!(!offers(pattern, "or"), "an infix is not an alternative");
    assert!(!offers(pattern, "rest"), "an absent word is absent");
}

/// …and the gates read the real patterns, which really do contain every entry —
/// so a gate that passes is passing on the grammar and not on an empty list.
#[test]
fn the_grammar_patterns_are_alternations_the_reader_can_see() {
    let grammar = grammar();
    for (rule, expected_at_least) in [("keywords", 17), ("capture-type", 10), ("parser-call", 14)] {
        let alts = alternatives(&rule_pattern(&grammar, rule));
        assert!(
            alts.len() >= expected_at_least,
            "`{rule}` reads as {} alternatives, expected at least {expected_at_least}: {alts:?}",
            alts.len()
        );
    }
}

/// **Gate 1.** Every keyword in `SyntaxKind`'s table appears in the grammar's
/// keyword pattern.
///
/// The source list is swept out of the kind space rather than written down
/// twice — a keyword added to `keyword_text` joins it by construction, and this
/// gate then requires the grammar to learn it too.
#[test]
fn every_keyword_is_in_the_grammars_keyword_pattern() {
    let grammar = grammar();
    let pattern = rule_pattern(&grammar, "keywords");
    let keywords = praxis_syntax::SyntaxKind::all_keyword_texts();
    assert!(
        keywords.len() >= 17,
        "the sweep found only {} keywords, which means it is not sweeping",
        keywords.len()
    );
    for keyword in keywords {
        assert!(
            offers(&pattern, keyword),
            "`{keyword}` is a Praxis keyword and the grammar's keyword pattern does not \
             offer it — a word that quietly stops being coloured is the failure this \
             gate exists for.\npattern: {pattern}"
        );
    }
}

/// **Gate 2.** Every §7.4 atomic appears in the capture-type pattern.
#[test]
fn every_atomic_is_in_the_grammars_capture_type_pattern() {
    let grammar = grammar();
    let pattern = rule_pattern(&grammar, "capture-type");
    for atomic in praxis_input_parser::AtomicKind::ALL {
        assert!(
            offers(&pattern, atomic.keyword()),
            "`{}` is a §7.4 atomic and the capture-type pattern does not offer it\npattern: {pattern}",
            atomic.keyword()
        );
    }
}

/// **Gate 3.** Every §7.5 constructor appears in the constructor pattern.
#[test]
fn every_constructor_is_in_the_grammars_constructor_pattern() {
    let grammar = grammar();
    let pattern = rule_pattern(&grammar, "parser-call");
    for ctor in praxis_input_parser::Constructor::ALL {
        assert!(
            offers(&pattern, ctor.keyword()),
            "`{}` is a §7.5 constructor and the constructor pattern does not offer it\npattern: {pattern}",
            ctor.keyword()
        );
    }
}

/// **Gate 4 — §6.2's agreement, enforced instead of documented.**
///
/// Every custom semantic token type in the server's legend has a
/// `semanticTokenScopes` entry, **and** that entry's scope is one the grammar
/// also emits. Without the second half the two layers would still be free to
/// paint the same construct two different colours, and the token would change
/// colour as the server attached.
#[test]
fn every_custom_semantic_token_maps_to_a_scope_the_grammar_emits() {
    let manifest = manifest();
    let grammar_text = read("syntaxes/praxis.tmLanguage.json");

    let mapping = manifest["contributes"]["semanticTokenScopes"]
        .as_array()
        .expect("the manifest contributes semanticTokenScopes");
    let praxis = mapping
        .iter()
        .find(|entry| entry["language"] == "praxis")
        .expect("a mapping for the `praxis` language");
    let scopes = praxis["scopes"]
        .as_object()
        .expect("the mapping is an object of token type → scopes");

    for token_type in praxis_lsp::semantic::CUSTOM_TOKEN_TYPES {
        let entry = scopes.get(*token_type).unwrap_or_else(|| {
            panic!(
                "the server's legend has the custom token type `{token_type}` and the \
                 extension maps no scope onto it: a theme would colour it as nothing, \
                 which is what §19.11 criterion 4 forbids"
            )
        });
        let list = entry
            .as_array()
            .unwrap_or_else(|| panic!("`{token_type}`'s mapping must be an array of scopes"));
        assert!(
            !list.is_empty(),
            "`{token_type}` maps to an empty scope list"
        );
        for scope in list {
            let scope = scope.as_str().expect("a scope is a string");
            assert!(
                grammar_text.contains(scope),
                "`{token_type}` maps to `{scope}`, which the grammar never emits — the two \
                 layers would paint the same construct differently and the colour would \
                 change as the server attached"
            );
        }
    }
}

/// The legend's four custom types are the four §19.11 criterion 4 names, so
/// gate 4 above is checking the right set rather than an empty one.
#[test]
fn the_custom_token_types_are_the_four_parser_classes() {
    assert_eq!(
        praxis_lsp::semantic::CUSTOM_TOKEN_TYPES,
        &[
            "parserConstructor",
            "parserTemplateText",
            "parserCaptureName",
            "parserCaptureType",
        ]
    );
    for name in praxis_lsp::semantic::CUSTOM_TOKEN_TYPES {
        assert!(
            praxis_lsp::semantic::TOKEN_TYPES.contains(name),
            "`{name}` must also be in the advertised legend"
        );
    }
}

/// **The extension invokes subcommands the CLI actually has.**
///
/// §19.11 criterion 5 is "VS Code run/check commands invoke the local Praxis
/// binary", and the way that breaks silently is the CLI renaming a subcommand
/// or a flag. `argv.ts` is read as text — no Node toolchain — and every literal
/// it names is checked against `praxis --help`'s own surface.
#[test]
fn the_extensions_argv_names_only_subcommands_the_cli_has() {
    let argv_ts = read("src/argv.ts");
    let cli_help = String::from_utf8(
        std::process::Command::new(bin_path())
            .arg("--help")
            .output()
            .expect("the binary runs")
            .stdout,
    )
    .expect("help is UTF-8");

    // The subcommands the extension's type declares.
    for subcommand in ["run", "check", "watch", "lsp"] {
        assert!(
            argv_ts.contains(&format!("\"{subcommand}\"")),
            "`argv.ts` should name the `{subcommand}` subcommand"
        );
        assert!(
            cli_help.contains(subcommand),
            "`argv.ts` invokes `praxis {subcommand}`, which `praxis --help` does not list"
        );
    }

    // …and the one flag it passes.
    assert!(
        argv_ts.contains("\"--input\""),
        "`argv.ts` passes `--input`"
    );
    let run_help = String::from_utf8(
        std::process::Command::new(bin_path())
            .args(["run", "--help"])
            .output()
            .expect("the binary runs")
            .stdout,
    )
    .expect("help is UTF-8");
    assert!(
        run_help.contains("--input"),
        "`argv.ts` passes `--input` to `praxis run`, which does not take it"
    );
}

/// The extension declares the four §15.4 commands, and the language it
/// contributes is the one the server's document selector answers for.
#[test]
fn the_extension_contributes_the_four_commands_and_the_px_language() {
    let manifest = manifest();
    let commands: Vec<&str> = manifest["contributes"]["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .map(|c| c["command"].as_str().expect("a command id"))
        .collect();
    for expected in [
        "praxis.runFile",
        "praxis.checkFile",
        "praxis.watchFile",
        "praxis.restartServer",
    ] {
        assert!(commands.contains(&expected), "missing `{expected}`");
    }

    let languages = manifest["contributes"]["languages"]
        .as_array()
        .expect("languages");
    let praxis = languages
        .iter()
        .find(|l| l["id"] == "praxis")
        .expect("the `praxis` language");
    let extensions: Vec<&str> = praxis["extensions"]
        .as_array()
        .expect("extensions")
        .iter()
        .map(|e| e.as_str().expect("a string"))
        .collect();
    assert_eq!(extensions, vec![".px"]);

    let grammars = manifest["contributes"]["grammars"]
        .as_array()
        .expect("grammars");
    assert_eq!(grammars[0]["scopeName"], "source.praxis");
    assert_eq!(
        grammars[0]["path"], "./syntaxes/praxis.tmLanguage.json",
        "the grammar the gates above read is the one the extension ships"
    );
}
