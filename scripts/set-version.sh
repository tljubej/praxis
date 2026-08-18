#!/usr/bin/env bash
#
# Set the workspace version.
#
#     ./scripts/set-version.sh 0.2.0
#
# ## Why this is a script and not an edit
#
# Every crate takes its version from `workspace.package.version`, so the release
# version lives in one place — but the *dependency* versions do not, and cannot.
# Cargo has no way to inherit a version into a dependency specification: a
# `[workspace.dependencies]` entry that a published crate depends on has to
# spell `version = "x.y.z"` out, because a dependency carrying only a path is
# rejected by the registry.
#
# So the number appears sixteen times in one file, and fifteen of those are easy
# to miss. Missing them is not a build error and not a test failure: the
# workspace still compiles, because a path dependency wins locally. It goes
# wrong only on the registry, where `praxis-cli 0.2.0` is published depending on
# `praxis-hir 0.1.0` — a real, resolvable, wrong version, permanently. This
# moves all sixteen together, and `scripts/publish.sh` refuses to publish if
# they ever disagree.
#
# `Cargo.lock` is refreshed too, so the tree stays `--locked`-clean.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <version>" >&2
    exit 2
fi

new=$1
if ! [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "error: '$new' is not a semver version" >&2
    exit 2
fi

old=$(cargo metadata --format-version 1 --no-deps |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')

if [[ "$old" == "$new" ]]; then
    echo "already at $new"
    exit 0
fi

# Two anchored substitutions rather than a blanket one, so a dependency that
# happens to share the old version number by coincidence — a third-party crate
# at "0.1.0" — is left alone. The first matches the sole top-level `version` key,
# which is `[workspace.package]`'s; the second matches only the internal
# `praxis-*` dependency lines. `praxis-test-support` carries no version and so
# matches neither, which is what keeps it unpublishable.
python3 - "$old" "$new" <<'PY'
import re, sys

old, new = sys.argv[1], sys.argv[2]
src = open("Cargo.toml").read()

src, n_ws = re.subn(r'(?m)^version = "%s"$' % re.escape(old), 'version = "%s"' % new, src)
src, n_dep = re.subn(
    r'(?m)^(praxis-[a-z-]+ = \{ path = "[^"]+", version = )"%s"' % re.escape(old),
    r'\g<1>"%s"' % new,
    src,
)

if n_ws != 1:
    sys.exit("error: expected exactly one [workspace.package] version, found %d" % n_ws)

open("Cargo.toml", "w").write(src)
print("workspace.package.version: %s -> %s" % (old, new))
print("internal dependency versions rewritten: %d" % n_dep)
PY

cargo update --workspace --quiet
echo "Cargo.lock refreshed"
echo
echo "next: review 'git diff', commit, then ./scripts/publish.sh --plan"
