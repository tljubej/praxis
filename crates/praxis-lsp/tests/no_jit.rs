//! §19.11 criterion 1, structurally: **the language server cannot reach the
//! JIT.**
//!
//! "Editing a typical puzzle file updates diagnostics without running JIT code"
//! is a claim about a code path, and a test that edits a file and observes that
//! nothing was compiled proves it for that one file on that one day. The
//! version that stays true is the one this file asserts: `praxis-lsp`'s manifest
//! does not depend on the crates that *are* the JIT, so no path through it can
//! reach one.
//!
//! The manifest is read at test time rather than quoted here — the
//! `crates/praxis-cli/tests/design_doc.rs` precedent: a test that quotes a file
//! can drift from it, a test that reads it cannot.

use std::path::PathBuf;

fn manifest() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("Cargo.toml");
    std::fs::read_to_string(&path).expect("praxis-lsp has a manifest")
}

/// The three crates that make a program run. `praxis-mir` lowers to a CFG,
/// `praxis-codegen-cranelift` emits machine code, `praxis-runtime` is the heap
/// and the ABI the emitted code calls into. None is reachable from a query.
const JIT_CRATES: &[&str] = &["praxis-mir", "praxis-codegen-cranelift", "praxis-runtime"];

#[test]
fn the_language_server_does_not_depend_on_the_jit() {
    let text = manifest();
    let deps = dependency_section(&text);
    for crate_name in JIT_CRATES {
        assert!(
            !deps.contains(crate_name),
            "`praxis-lsp` must not depend on `{crate_name}`: §19.11 criterion 1 \
             requires diagnostics without running JIT code, and this manifest is \
             what makes that true by construction rather than by observation"
        );
    }
}

/// …and it *does* depend on the front end, so the test above is not passing
/// because the manifest is empty.
#[test]
fn the_language_server_does_depend_on_the_front_end() {
    let text = manifest();
    let deps = dependency_section(&text);
    for crate_name in ["praxis-parser", "praxis-hir", "praxis-source"] {
        assert!(
            deps.contains(crate_name),
            "`praxis-lsp` must depend on `{crate_name}` to answer anything"
        );
    }
}

/// The `[dependencies]` and `[dev-dependencies]` bodies, without the comment
/// block above them — which names the three crates on purpose and would
/// otherwise make the assertion vacuously fail.
fn dependency_section(text: &str) -> String {
    let mut out = String::new();
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]" || trimmed == "[dev-dependencies]";
            continue;
        }
        if in_deps && !trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
