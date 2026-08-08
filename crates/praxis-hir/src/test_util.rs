//! Helpers shared by this crate's test modules: interning and parsing a
//! fixture, the parse → analyze → lower preamble, and finding a lowered `fn`
//! by name.
//!
//! These stay here rather than in `praxis-test-support`: that crate does not
//! depend on `praxis-hir` or `praxis-parser`, and the dependency edge already
//! runs the other way.

#![cfg(test)]

use praxis_ast::{AstNode, SourceFile};
use praxis_parser::{parse, ParseOutput};
use praxis_source::FileId;
use praxis_test_support::single_file;

use crate::{analyze_root, Analysis, TypedFn, TypedItem, TypedModule};

/// The name every fixture is interned under. Nothing asserts on it.
const TEST_FILE: &str = "test.px";

/// Intern `text` as a whole file and parse it.
pub(crate) fn parse_file(text: &str) -> (FileId, ParseOutput) {
    let (_map, id) = single_file(TEST_FILE, text);
    (id, parse(id, text))
}

/// Parse and analyze `text` — the front end `praxis check` and the editor run.
pub(crate) fn analyze(text: &str) -> Analysis {
    let (id, parsed) = parse_file(text);
    analyze_root(id, &parsed.tree)
}

/// [`analyze`] plus typed-HIR lowering, keeping the parse output for the
/// fixtures whose first assertion is "this program still parses".
pub(crate) fn parse_analyze_and_lower(text: &str) -> (ParseOutput, Analysis, TypedModule) {
    let (id, parsed) = parse_file(text);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = SourceFile::cast(parsed.tree.clone()).expect("a source file");
    let module = crate::lower::lower(id, &root, &mut analysis);
    (parsed, analysis, module)
}

/// [`parse_analyze_and_lower`] without the parse output.
///
/// Lowering runs checks analysis does not (exhaustiveness, closure capture), so
/// tests reach for this when they need those. `lower` destructures
/// `diagnostics: _` and never pushes to it, so everything it reports lands in
/// `module.diagnostics` and a test asserting on `analysis.diagnostics` is
/// unaffected by the extra pass.
pub(crate) fn analyze_and_lower(text: &str) -> (Analysis, TypedModule) {
    let (_, analysis, module) = parse_analyze_and_lower(text);
    (analysis, module)
}

/// The lowered `fn` named `name`, panicking when the module has no such item.
///
/// Test-local on purpose: `TypedModule` is only re-exported from `lib.rs`, and
/// the production searches (`praxis-cli`'s runner, `praxis-debugger`'s
/// evaluator) choose an item under rules of their own.
pub(crate) fn fn_named<'m>(module: &'m TypedModule, name: &str) -> &'m TypedFn {
    module
        .items
        .iter()
        .find_map(|item| match item {
            TypedItem::Fn(f) if f.name == name => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no `{name}` item in the lowered module"))
}

/// [`fn_named`] for the item a file's top-level statements are lowered into.
/// Its name is [`ENTRY_NAME`](crate::ENTRY_NAME), which no source file can
/// spell.
pub(crate) fn entry_fn(module: &TypedModule) -> &TypedFn {
    fn_named(module, crate::ENTRY_NAME)
}
