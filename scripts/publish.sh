#!/usr/bin/env bash
#
# Publish the workspace to crates.io.
#
# Three modes, and the default is the one that uploads:
#
#     ./scripts/publish.sh --plan      # order + per-crate registry status; reads only
#     ./scripts/publish.sh --dry-run   # package and build all 15, upload nothing
#     ./scripts/publish.sh             # publish, skipping what is already up
#
# ## The order is computed, never listed
#
# Fifteen crates depend on each other and crates.io rejects a package whose
# dependency is not already on the registry, so they go up in topological order.
# That order is derived from `cargo metadata` on every run. A hand-maintained
# list is a list that goes stale the first time a crate gains a dependency, and
# the failure it produces arrives fifteen minutes into an upload that cannot be
# undone.
#
# **A versioned dev-dependency is an edge like any other.** Cargo strips a
# path-only dev-dependency from the manifest it ships, but keeps one that
# carries a version — and the registry then checks that it exists. So
# `praxis-ast` waits for `praxis-parser`, which is a dev-dependency and nothing
# else. `praxis-test-support` sets `publish = false` and is depended on without a
# version, which is what keeps it off the registry without stranding the nine
# crates that test with it.
#
# ## Skipping is what makes a failure survivable
#
# Every crate is checked against the registry before it is published, and one
# already at this version is skipped. An upload that dies at crate 12 is then
# resumed by re-running this script, rather than by editing a list of the three
# that are left. This matters more than it sounds like it should: see the rate
# limit below, which makes a first publish take hours and gives it plenty of
# time to be interrupted.
#
# ## The new-crate rate limit is the reason this takes an afternoon
#
# crates.io meters *new crate names* separately from new versions of a name it
# already knows: a burst of five, then one per ten minutes. A workspace with
# fifteen unpublished names spends about one hundred minutes waiting, and no
# flag shortens it. This script waits it out rather than failing — it sleeps and
# retries when the registry says that is why it refused, so `--plan` before and
# a walk away during is the whole workflow.
#
# Subsequent releases do not pay it. Once every name exists, the limit is the
# new-version one: a burst of thirty, then one per minute.
set -euo pipefail

cd "$(dirname "$0")/.."

MODE=publish
case "${1:-}" in
    --plan) MODE=plan ;;
    --dry-run) MODE=dry-run ;;
    "") ;;
    *)
        echo "usage: $0 [--plan|--dry-run]" >&2
        exit 2
        ;;
esac

# How long to wait when the registry refuses for rate-limiting reasons, and how
# many times. Ten minutes is the documented refill for a new crate name; the
# extra thirty seconds is slack, because the bucket is refilled on the server's
# clock and not on ours. Twenty attempts is above what fifteen names can need.
RATE_LIMIT_SLEEP=630
RATE_LIMIT_RETRIES=20

# ---------------------------------------------------------------------------
# The plan: publishable crates in dependency order, one per line.
#
# `publish` in `cargo metadata` is null when publishing is allowed and a list of
# permitted registries when it is restricted; `publish = false` shows up as the
# empty list, which is how `praxis-test-support` excludes itself.
# ---------------------------------------------------------------------------
plan() {
    cargo metadata --format-version 1 --no-deps | python3 -c '
import json, sys

pkgs = {p["name"]: p for p in json.load(sys.stdin)["packages"]}
names = set(pkgs)
publishable = {n for n in names if pkgs[n].get("publish") != []}

# An edge is anything that survives into the shipped manifest: normal and build
# dependencies always, a dev-dependency only when it carries a version.
edges = {}
for n in publishable:
    deps = set()
    for d in pkgs[n]["dependencies"]:
        if d["name"] not in publishable:
            continue
        if d["kind"] in (None, "build") or (d["kind"] == "dev" and d["req"] != "*"):
            deps.add(d["name"])
    edges[n] = deps

order, remaining = [], dict(edges)
while remaining:
    ready = sorted(n for n, d in remaining.items() if not d - set(order))
    if not ready:
        sys.exit("dependency cycle among: %s" % ", ".join(sorted(remaining)))
    order.extend(ready)
    for n in ready:
        del remaining[n]

for n in order:
    print(n, pkgs[n]["version"])
'
}

# ---------------------------------------------------------------------------
# Preflight. Everything here is cheap and every one of them has been the reason
# a release went out wrong somewhere.
# ---------------------------------------------------------------------------

# The workspace version and the versions the internal dependencies pin must
# agree. Cargo cannot inherit a version into a dependency specification — a
# `[workspace.dependencies]` entry has to write `version = "x.y.z"` out — so
# bumping `workspace.package.version` alone publishes new crates that depend on
# the old ones. `scripts/set-version.sh` moves both together; this refuses to
# publish if something moved only one.
check_versions() {
    local ws_version mismatched
    ws_version=$(cargo metadata --format-version 1 --no-deps |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')

    mismatched=$(grep -n '^praxis-[a-z-]* = { path = ' Cargo.toml |
        grep -v "version = \"$ws_version\"" |
        grep 'version = ' || true)

    if [[ -n "$mismatched" ]]; then
        echo "error: internal dependencies pin a version other than $ws_version:" >&2
        echo "$mismatched" >&2
        echo "  run ./scripts/set-version.sh $ws_version" >&2
        exit 1
    fi
}

check_clean_tree() {
    if [[ -n "$(git status --porcelain)" ]]; then
        echo "error: working tree is dirty; commit or stash before publishing" >&2
        git status --short >&2
        exit 1
    fi
}

# crates.io asks for a User-Agent that identifies the caller and will refuse a
# request without one.
UA="praxis-publish (https://github.com/tljubej/praxis)"

# 0 when this exact version is on the registry. A network failure must not read
# as "not published" — that would publish something twice — so anything that is
# neither a clear yes nor a clear no stops the run.
#
# **There are two distinct ways to be absent and both have to be spelled out.**
# A name the registry has never seen answers "crate `x` does not exist"; a name
# it knows, at a version it does not, answers "crate `x` does not have a version
# `y`". The first is what a first release sees and the second is what every
# release after it sees, so recognising only the first would abort the run on
# all fifteen crates the next time the version is bumped.
is_published() {
    local crate=$1 version=$2 body
    body=$(curl -sS -A "$UA" "https://crates.io/api/v1/crates/$crate/$version") || {
        echo "error: could not reach crates.io to check $crate $version" >&2
        exit 1
    }
    case "$body" in
        *'"num":"'"$version"'"'*) return 0 ;;
        *'does not exist'* | *'does not have a version'* | *'Not Found'*) return 1 ;;
        *)
            echo "error: unexpected crates.io response for $crate $version:" >&2
            echo "  $body" >&2
            exit 1
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Modes
# ---------------------------------------------------------------------------

if [[ "$MODE" == plan ]]; then
    check_versions
    printf '%-28s %-8s %s\n' CRATE VERSION STATUS
    n=0
    while read -r crate version; do
        n=$((n + 1))
        if is_published "$crate" "$version"; then
            printf '%-28s %-8s already published\n' "$crate" "$version"
        else
            printf '%-28s %-8s to publish\n' "$crate" "$version"
        fi
    done < <(plan)
    echo
    echo "$n crates in dependency order"
    exit 0
fi

if [[ "$MODE" == dry-run ]]; then
    check_versions
    # The whole workspace at once, and deliberately not a loop of
    # `cargo publish -p X --dry-run`: a single-package dry run resolves that
    # package's dependencies against the real registry, so on a workspace whose
    # crates are not published yet every one of them fails on the crate before
    # it. `--workspace` builds a temporary local registry from the packaged
    # tarballs and verifies against that, which is the thing worth checking —
    # each crate compiling from what it actually ships.
    exec cargo publish --workspace --dry-run --locked
fi

# ---------------------------------------------------------------------------
# The real thing.
# ---------------------------------------------------------------------------
check_clean_tree
check_versions

while read -r crate version; do
    if is_published "$crate" "$version"; then
        echo "== $crate $version already published, skipping"
        continue
    fi

    echo "== publishing $crate $version"
    attempt=1
    while true; do
        # The command runs **once** per attempt and its output is captured, then
        # inspected. Deciding whether to retry by running `cargo publish` a
        # second time would risk uploading the crate twice, which is the one
        # mistake here that cannot be taken back.
        set +e
        output=$(cargo publish -p "$crate" --locked 2>&1)
        status=$?
        set -e
        printf '%s\n' "$output"

        if ((status == 0)); then
            break
        fi

        # The registry's own words are the only reliable signal that this was a
        # rate limit rather than a real rejection, and cargo passes them
        # through. Retrying anything else would turn a bad manifest into a
        # twenty-attempt loop.
        if ! grep -qiE 'too many|rate limit|429' <<<"$output"; then
            echo "error: publishing $crate failed; fix it and re-run this script" >&2
            echo "  everything already uploaded is skipped on the next run" >&2
            exit 1
        fi
        if ((attempt >= RATE_LIMIT_RETRIES)); then
            echo "error: $crate still rate-limited after $attempt attempts" >&2
            exit 1
        fi
        echo "-- rate limited; waiting ${RATE_LIMIT_SLEEP}s (attempt $attempt)"
        sleep "$RATE_LIMIT_SLEEP"
        attempt=$((attempt + 1))
    done
done < <(plan)

echo "published; https://crates.io/crates/praxis-cli"
