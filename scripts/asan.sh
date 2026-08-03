#!/usr/bin/env bash
#
# Run the whole workspace test suite under AddressSanitizer.
#
# The command is one line and it is `docs/handovers/25-two-mallocs-per-runtime-call.md` §1:
#
#     RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS=detect_leaks=0 \
#         cargo +nightly test --workspace --target <host-triple> --release
#
# ## `--target` is the whole trick
#
# Without it, cargo applies `RUSTFLAGS` to *host* artifacts too — build scripts
# and proc macros. Naming the target explicitly splits the graph: target
# artifacts are instrumented, host tooling is not. Four separate packages
# rediscovered that independently while scoping handover 26, which is why it is
# written down here rather than left in a handover section.
#
# The failure was reproduced on a two-crate probe to pin the mechanism, because
# "the build fails" is not enough to recognize it by. **It is the proc macro, not
# the build script.** A trivial `build.rs` builds fine instrumented; a
# `proc-macro = true` dylib does not, because rustc `dlopen`s it into its own
# uninstrumented process:
#
#     ==53367==ERROR: Interceptors are not working. This may be because
#     AddressSanitizer is loaded too late (e.g. via dlopen).
#     error: could not compile `…` (lib test)
#     Caused by: process didn't exit successfully: `… -Zsanitizer=address`
#       (signal: 6, SIGABRT: process abort signal)
#
# This workspace reaches that through `clap`'s and `serde`'s derives. If you see
# an ASan abort *during compilation* rather than during a test, `--target` is
# missing.
#
# The triple is derived from the nightly toolchain rather than hardcoded, so the
# same script works at the keyboard (`aarch64-apple-darwin`) and on hosted CI
# (`x86_64-unknown-linux-gnu`, see `.github/workflows/ci.yml`).
#
# ## The baseline
#
# At `e4f42e6`, on Apple M2 Pro / macOS 26.6, nightly `rustc 1.99.0-nightly`:
# **1911 passed, 0 failed, 0 AddressSanitizer reports, across 28 test binaries.**
# That is the number a run of this script is compared against. A drop in the
# count is as interesting as a report: it means a test binary did not build.
#
# **Instrumentation was verified rather than assumed**, and this script re-does
# that verification every run instead of trusting the flag. A build that silently
# dropped `-Zsanitizer=address` passes 1911 tests just as happily and proves
# nothing. The evidence, recorded at `e4f42e6`: each test binary links
# `@rpath/librustc-nightly_rt.asan.dylib` and carries **37 undefined `__asan_*`
# symbols**. On Linux the runtime is linked statically, so the symbols are
# defined rather than undefined — hence the check below asks only whether
# `__asan_*` symbols are present at all.
#
# ## What this does NOT cover — read this before believing a green run
#
# **JIT-generated code.** Cranelift emits machine code straight into a mapped
# region; there is no compilation unit for rustc to instrument and no `-Z` flag
# that reaches it. So ASan sees the runtime — `claim_free_block`, `block_index`,
# `relink_pages`, the sweep, every payload accessor, the parser interpreter — and
# is blind to everything the backend emits.
#
# Handover 26 §7 trap 6: **W4b, W10 and W8-S0b all put new unsafe behaviour
# exactly there.** W4b hands generated code a raw buffer pointer, W10 has
# generated code writing `GcHeader`s, and W8-S0b makes the collector skip a slot
# holding an f64 bit pattern. For those three a green run here is **necessary and
# not sufficient**, and the soundness argument has to be written down in the ADR
# as well as run.
#
# ## Why this is not in `just ci`
#
# The instrumented build is a second full compile of the workspace and `just ci`
# is already ~17 minutes on this laptop. It runs on a schedule instead — see
# `.github/workflows/asan.yml` and the note in the `justfile`.

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v rustup >/dev/null 2>&1; then
    echo "asan: rustup not found — the sanitizer needs a nightly toolchain" >&2
    exit 1
fi
if ! rustup toolchain list | grep -q '^nightly'; then
    echo "asan: no nightly toolchain — run 'rustup toolchain install nightly'" >&2
    echo "asan: (rust-toolchain.toml pins stable; '+nightly' below overrides it)" >&2
    exit 1
fi

TARGET="$(rustc +nightly -vV | sed -n 's/^host: //p')"
if [ -z "$TARGET" ]; then
    echo "asan: could not read the host triple from 'rustc +nightly -vV'" >&2
    exit 1
fi

export RUSTFLAGS="-Zsanitizer=address"
export ASAN_OPTIONS="detect_leaks=0"

echo "asan: target $TARGET, RUSTFLAGS=$RUSTFLAGS, ASAN_OPTIONS=$ASAN_OPTIONS" >&2

# Compile and link first, so the binaries exist to be inspected. The `cargo test`
# below re-uses this build byte for byte — same flags, same target, same profile
# — so the second invocation links nothing.
echo "asan: building instrumented test binaries" >&2
# stderr is deliberately left attached: `--message-format=json` moves the
# machine-readable stream to stdout, so progress and any compile error still
# reach the terminal. Swallowing it would turn a build failure into a bare
# nonzero exit with nothing to read.
BINARIES="$(
    cargo +nightly test --workspace --target "$TARGET" --release --no-run \
        --message-format=json |
        sed -n 's/.*"executable":"\([^"]*\)".*/\1/p'
)"

if [ -z "$BINARIES" ]; then
    echo "asan: cargo produced no test binaries — nothing to verify or run" >&2
    exit 1
fi

# Verify the instrumentation is real. A build that quietly dropped the flag is
# the failure mode this guards: it is green, it is fast, and it proves nothing.
COUNT=0
UNINSTRUMENTED=""
while IFS= read -r exe; do
    [ -n "$exe" ] || continue
    COUNT=$((COUNT + 1))
    # `grep`, not `grep -q`. `-q` exits at the first match, `nm` then dies of
    # SIGPIPE with 141, and `set -o pipefail` two dozen lines up turns that into
    # the pipeline's status — so the check reports "not instrumented" for
    # precisely the binaries big enough for `nm` to still be writing, which is
    # all the interesting ones. It is a race, so it passed at `e4f42e6` and
    # fails on a tree whose `praxis` has 25k `__asan_*` symbols. Found by W4a
    # (ADR-118), whose sanitizer run is not optional.
    if ! nm "$exe" 2>/dev/null | grep '__asan_' >/dev/null; then
        UNINSTRUMENTED="$UNINSTRUMENTED  $exe"$'\n'
    fi
done <<EOF
$BINARIES
EOF

if [ -n "$UNINSTRUMENTED" ]; then
    echo "asan: these binaries carry no __asan_* symbols and are NOT instrumented:" >&2
    printf '%s' "$UNINSTRUMENTED" >&2
    echo "asan: refusing to report a pass that would prove nothing" >&2
    echo "asan: (if 'nm' says 'no symbols', the binary was stripped and this" >&2
    echo "asan:  check is blind rather than the build being uninstrumented —" >&2
    echo "asan:  [profile.release] sets debug = 0 and no strip, so it should not be)" >&2
    exit 1
fi
# Deliberately not compared against 28: cargo also reports the `praxis` bin
# itself here, and "N test binaries" is a `cargo test` output line, not this
# count. The claim being made is "all of them are instrumented", nothing more.
echo "asan: $COUNT executables produced, all instrumented" >&2

echo "asan: running the suite (e4f42e6 baseline: 1911 passed, 0 failed, 0 reports)" >&2
cargo +nightly test --workspace --target "$TARGET" --release
