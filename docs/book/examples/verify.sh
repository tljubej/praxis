#!/usr/bin/env bash
#
# Re-run every example in the book and diff it against the output printed in the
# chapter that quotes it.
#
# Every code block in the book that shows a program *and* its output is backed by
# a real file under this directory. This script is what makes that claim
# checkable: it walks the tree rather than listing it, so an example is covered
# the moment it lands.
#
# An example is a `.px` file plus one expectation file, and which expectation it
# has is what says how the example is meant to be run:
#
#   name.out      expected stdout of `praxis run` (exit 0). `name.in` is passed
#                 as `--input` when it exists.
#   name.fault    expected stderr of `praxis run --debug never` (exit 1) — a
#                 program that compiles and then faults at run time.
#   name.fault-head   the same, but only the leading lines are compared. For the
#                 one fault whose full report is thousands of near-identical
#                 frames: the recursion limit prints its whole stack, and storing
#                 8000 lines of `#N  down` to check one message is a bad trade.
#                 Use it only when the tail is genuinely uninteresting.
#   name.err      expected stderr of `praxis check` (exit 1) — a program that
#                 does not get as far as running.
#   name.session  expected stdout+stderr of `praxis run --debug always` driven by
#                 the debugger commands in `name.cmds` on stdin.
#
# A `.px` with none of these is an error, not a skip: a skipped example is
# exactly what this script exists to prevent.
#
# Usage:
#   docs/book/examples/verify.sh                    # check every example
#   docs/book/examples/verify.sh --bless            # rewrite expectations from reality
#   docs/book/examples/verify.sh --bless input      # ...only under examples/input/
#   PRAXIS=path/to/praxis docs/book/examples/verify.sh
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
# Prefer the release binary and fall back to the debug one, so this is runnable
# straight after `just test` without a second full link.
praxis="${PRAXIS:-}"
if [[ -z "$praxis" ]]; then
    for candidate in "$root/target/release/praxis" "$root/target/debug/praxis"; do
        [[ -x "$candidate" ]] && { praxis="$candidate"; break; }
    done
    praxis="${praxis:-$root/target/release/praxis}"
fi

bless=0
if [[ "${1:-}" == "--bless" ]]; then
    bless=1
    shift
fi

# An optional subdirectory narrows the walk. Blessing a single area is how one
# author avoids rewriting an expectation somebody else is still working on.
scope="$here"
if [[ -n "${1:-}" ]]; then
    scope="$here/${1%/}"
    if [[ ! -d "$scope" ]]; then
        echo "no such example directory: $scope" >&2
        exit 2
    fi
fi

if [[ ! -x "$praxis" ]]; then
    echo "no praxis binary at $praxis — run: cargo build --release -p praxis-cli" >&2
    exit 2
fi

pass=0
fail=0
failed_names=()

# Compare actual against the expectation file, or write it under --bless. A
# fourth argument is a line count: only that many leading lines are compared,
# which is what backs the `.fault-head` form.
check() {
    local label="$1" expected_file="$2" actual="$3" head_lines="${4:-0}"
    if (( head_lines )); then
        actual="$(printf '%s\n' "$actual" | head -n "$head_lines")"
    fi
    if (( bless )); then
        printf '%s' "$actual" >"$expected_file"
        echo "blessed  $label"
        (( pass++ ))
        return
    fi
    local expected
    expected="$(cat "$expected_file")"
    if [[ "$expected" == "$actual" ]]; then
        (( pass++ ))
    else
        (( fail++ ))
        failed_names+=("$label")
        echo "FAIL     $label"
        diff <(printf '%s' "$expected") <(printf '%s' "$actual") | sed 's/^/         /'
    fi
}

# Every example runs from its own directory with a bare filename, because the
# filename is in the output: a diagnostic and a fault report both print the path
# they were given. Invoking by absolute path would write this checkout's location
# into every expectation file, and from there into every diagnostic quoted in the
# book. Change directory first, and what a reader sees in a chapter is what that
# reader gets by running the example the same way.
run_example() {
    local dir="$1"
    shift
    (cd "$dir" && "$praxis" "$@")
}

while IFS= read -r -d '' px; do
    base="${px%.px}"
    label="${px#"$root"/}"
    dir="$(dirname "$px")"
    file="$(basename "$px")"
    stem="$(basename "$base")"
    input=()
    [[ -f "$base.in" ]] && input=(--input "$stem.in")

    # Under --bless, a `.px` with no expectation yet is a new success case:
    # create the `.out` and fill it below. An example meant to fail declares
    # that by having an (even empty) `.fault`, `.err` or `.session` file, which
    # is the only way to say which of the four ways to run it is intended.
    if (( bless )) && [[ ! -f "$base.out" && ! -f "$base.fault" && ! -f "$base.fault-head" && ! -f "$base.err" && ! -f "$base.session" ]]; then
        : >"$base.out"
    fi

    if [[ -f "$base.out" ]]; then
        actual="$(run_example "$dir" run "$file" ${input[@]+"${input[@]}"} --debug never 2>/dev/null)"
        status=$?
        if (( status != 0 )); then
            (( fail++ ))
            failed_names+=("$label")
            echo "FAIL     $label — expected success, exited $status"
            run_example "$dir" run "$file" ${input[@]+"${input[@]}"} --debug never 2>&1 >/dev/null | sed 's/^/         /'
            continue
        fi
        check "$label" "$base.out" "$actual"

    elif [[ -f "$base.fault" ]]; then
        actual="$(run_example "$dir" run "$file" ${input[@]+"${input[@]}"} --debug never 2>&1 >/dev/null)"
        check "$label" "$base.fault" "$actual"

    elif [[ -f "$base.fault-head" ]]; then
        actual="$(run_example "$dir" run "$file" ${input[@]+"${input[@]}"} --debug never 2>&1 >/dev/null)"
        # How many leading lines to compare is however many the expectation
        # already holds. A fresh one is seeded by writing the lines you want.
        head_lines="$(wc -l <"$base.fault-head" | tr -d ' ')"
        (( head_lines )) || head_lines=1
        check "$label" "$base.fault-head" "$actual" "$head_lines"

    elif [[ -f "$base.err" ]]; then
        actual="$(run_example "$dir" check "$file" --color never 2>&1 >/dev/null)"
        check "$label" "$base.err" "$actual"

    elif [[ -f "$base.session" ]]; then
        if [[ ! -f "$base.cmds" ]]; then
            (( fail++ ))
            failed_names+=("$label")
            echo "FAIL     $label — has .session but no .cmds to drive it"
            continue
        fi
        actual="$(run_example "$dir" run "$file" ${input[@]+"${input[@]}"} --debug always <"$base.cmds" 2>&1)"
        check "$label" "$base.session" "$actual"

    else
        (( fail++ ))
        failed_names+=("$label")
        echo "FAIL     $label — no .out/.fault/.fault-head/.err/.session expectation"
    fi
done < <(find "$scope" -name '*.px' -print0 | sort -z)

echo
echo "$pass ok, $fail failed"
if (( fail )); then
    printf '  %s\n' "${failed_names[@]}"
    exit 1
fi
