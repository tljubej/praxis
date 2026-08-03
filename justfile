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

# AddressSanitizer over the whole suite. This is deliberately NOT a dependency
# of `ci`.
#
# The instrumented build is a *second full compile* of the workspace, and `ci` is
# already ~17 minutes on the development laptop — of which ~14 is macOS XProtect
# exec-scanning freshly linked binaries. Doubling that makes the pre-push gate
# one people stop running, which costs more than the sanitizer catches. So it
# runs nightly instead: `.github/workflows/asan.yml`, on a schedule, calling this
# same recipe. ADR-002's rule that hosted CI runs what developers run is intact —
# what is new is a second *job*, not a second command.
#
# Needs a nightly toolchain (`rustup toolchain install nightly`);
# `rust-toolchain.toml` pins stable and the script overrides it with `+nightly`.
# Why the flags are what they are is in the script, at length.
#
# It does not cover JIT-generated code: Cranelift emits that raw and no `-Z` flag
# reaches it. A green run is necessary and not sufficient for any change that
# puts new unsafe behaviour in generated code.
#
# Run the whole suite under AddressSanitizer (nightly toolchain; not in `ci`).
asan:
    ./scripts/asan.sh
