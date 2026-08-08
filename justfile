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
# and the result is never cached — so finding zero still costs ~6s per crate on
# every run. A `///` example is still compiled by `cargo doc`; it is never
# executed, so assertions belong in unit tests. See the README.
test:
    cargo test --workspace

# Build every crate.
build:
    cargo build --workspace

# The full quality gate — exactly what hosted CI runs. Run this before pushing.
#
# `book-verify` is last because it needs the binary `book-binary` links, and
# that link is the most expensive step in the gate. See the book section below
# for what it covers.
ci: fmt-check clippy test book-binary book-verify
    @echo "all Praxis checks passed"

# AddressSanitizer over the whole suite. This is deliberately NOT a dependency
# of `ci`.
#
# The instrumented build is a *second full compile* of the workspace, and `ci` is
# already ~17 minutes on the development laptop — of which ~14 is macOS XProtect
# exec-scanning freshly linked binaries. Doubling that makes the pre-push gate
# one people stop running, which costs more than the sanitizer catches. So it
# runs nightly instead: `.github/workflows/asan.yml`, on a schedule, calling this
# same recipe — a second *job*, not a second command, so ADR-002's rule that
# hosted CI runs what developers run still holds.
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

# --- The book -----------------------------------------------------------------
#
# `docs/book` is an mdBook. Its examples are not decoration: every code block
# that shows a program *and* its output is a real file under
# `docs/book/examples/`, and `book-verify` re-runs all of them against the
# compiler in this tree and diffs the result against what the chapter prints.
#
# **`book-verify` is part of `ci`**, and it is not there for documentation
# tidiness: it is the only end-to-end coverage the crash debugger's expression
# evaluator has, and the link it costs is one binary the developer building this
# tree wanted anyway. `cargo test` can be green while the examples are broken.
#
# `book-bless` is deliberately *not* in `ci` — see its own note.

# Render the book to docs/book/book (gitignored).
book:
    cd docs/book && mdbook build

# Serve the book with live reload on http://localhost:3000.
book-serve:
    cd docs/book && mdbook serve --open

# Build the binary `book-verify` runs the examples with.
#
# Its own recipe rather than a line inside `book-verify`, so running the check
# by hand against a binary you already have — `PRAXIS=… just book-verify` — does
# not force a rebuild. `ci` depends on both, in order.
#
# Debug and not release: `verify.sh` prefers `target/release/praxis` when one
# exists, and this is a correctness gate rather than a timing one, so paying for
# an optimized build would buy the gate nothing.
book-binary:
    cargo build -p praxis-cli

# Re-run every example in the book and diff it against the chapters.
# Needs a built `praxis` — `just book-binary`, or set PRAXIS to one you have.
#
# **The binary is pinned, and it has to be.** `verify.sh`'s own discovery
# prefers `target/release/praxis` over the debug one, which for a gate is a
# silent wrong answer waiting to happen: a release binary left behind by an
# earlier build checks the book against a compiler that is not this tree's, and
# the failure it reports is about the wrong program. An explicit `PRAXIS` still
# wins, so `PRAXIS=… just book-verify` works.
book-verify:
    PRAXIS="${PRAXIS:-$PWD/target/debug/praxis}" ./docs/book/examples/verify.sh

# Rewrite the book's expectation files from what the compiler actually prints.
# Review the diff: this is how a real regression gets papered over.
book-bless:
    ./docs/book/examples/verify.sh --bless
