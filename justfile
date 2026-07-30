# Praxis quality gate.
#
# The aggregate `ci` recipe is the single source of truth: hosted CI runs
# `just ci` and so should you. `fmt` modifies files and is therefore NOT part
# of `ci` — CI only verifies. See docs/decisions/002-ci-via-just.md.

# Default: show the available recipes.
default:
    @just --list

# Reformat the code. Modifies files; NOT part of `ci`.
fmt:
    cargo fmt

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --check

# Lint the whole workspace. `-D warnings` makes any clippy hit a hard error.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the whole test suite.
#
# Doctests are NOT part of this, and not part of `ci`: every library crate sets
# `doctest = false`. `cargo test` runs `rustdoc --test` per crate, rustdoc must
# analyze the whole crate before it can find out how many doctests are in it,
# and the result is never cached — so finding zero cost ~6s per crate on every
# run, 95s of the suite to execute the one doctest the workspace had. A `///`
# example is still compiled by `cargo doc`; it is never executed, so assertions
# belong in unit tests. See the README.
test:
    cargo test --workspace

# Build every crate.
build:
    cargo build --workspace

# The full quality gate — exactly what hosted CI runs. Run this before pushing.
ci: fmt-check clippy test
    @echo "all Praxis checks passed"
