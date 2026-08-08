//! The three paths every `praxis-cli` integration test starts from: the binary
//! under test, the workspace root, and this crate's fixture tree.
//!
//! **Why they live here and not in `praxis-test-support`.**
//! `CARGO_BIN_EXE_praxis` is set by cargo only while compiling *this* package's
//! integration tests, so `bin_path` has to expand the literal inside one of
//! them; a helper crate would see no such variable. The other two follow it
//! rather than being split across two homes.

// Every test binary compiles this whole module and calls part of it, so the
// items a given file does not use are dead *there* and nowhere else — and
// `just ci` runs clippy with `-D warnings`.
#![allow(dead_code)]

use std::path::PathBuf;

/// The compiled `praxis` binary.
///
/// `CARGO_BIN_EXE_praxis` is set by cargo when running integration tests and
/// points at the compiled `praxis` binary.
pub fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_praxis"))
}

/// The workspace root, from this crate's manifest directory.
pub fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/praxis-cli -> crates
    p.pop(); // crates -> workspace root
    p
}

/// A path under this crate's `tests/fixtures`.
pub fn fixture(relative: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(relative);
    p
}
