#!/usr/bin/env python3
"""Re-record every terminal on the website from the binary in this tree.

    python3 www/tools/capture.py [--praxis target/release/praxis]

Everything the page shows as terminal output is produced here and inlined into
`www/index.html`, so the site cannot drift from what the compiler prints. Three
stages:

  1. `record.py` drives the crash debugger on a **sized pty** and logs its raw
     output with timestamps. A pipe would take the line-oriented prompt instead
     of the full-screen TUI, and `script(1)` reports a 0x0 window, so the pty
     and its `TIOCSWINSZ` are the point rather than an implementation detail.
  2. `vt.py` replays that stream through a small VT emulator and emits one
     row-diff per redraw.
  3. `pack.py` interns the SGR attributes and writes the blob the page reads.

The `.px` files under `demos/` are the programs shown on the page. Two of them
are copies of book examples (`tiles.px`, `maze.px`) and the rest are written for
the site; all of them are run, not transcribed.
"""
import argparse
import json
import pathlib
import re
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent
DEMOS = HERE / "demos"
PAGE = HERE.parent / "index.html"

OPEN = '<script id="demo-data" type="application/json">'
CLOSE = "</script>"

# The plain (non-interactive) terminals on the page, in the order they appear.
CAPTURES = {
    "segments": ["run", "segments.px", "--input", "segments.in"],
    "typo": ["check", "typo.px"],
    "points": ["run", "points.px", "--input", "points.in", "--debug", "never"],
    "tiles": ["check", "tiles.px"],
    "maze": ["run", "maze.px", "--input", "maze.in"],
}

# The debugger recording. 104x32 is wide enough that the source pane shows a
# real line and narrow enough to fit the page's column at a legible size.
TUI_COLS, TUI_ROWS = 104, 32

# The widest line a snippet may have. It is what fits a feature card at the
# page's smallest two-column width without wrapping.
SNIPPET_COLS = 50
TUI_CMD = ["run", "walkthrough.px", "--input", "walkthrough.in", "--debug", "always"]


def run_px(praxis, path):
    """Run one demo program against its `.in`, if it has one."""
    argv = [praxis, "run", path.name, "--debug", "never"]
    if path.with_suffix(".in").exists():
        argv += ["--input", path.with_suffix(".in").name]
    p = subprocess.run(argv, cwd=path.parent, capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def check_snippets(praxis, html):
    """Run every program the page quotes and paste the source back into it.

    The page shows these in full rather than as excerpts, and the text it shows
    is written here from the file that just ran — so a snippet cannot be stale,
    and one that stops working stops the build instead of shipping.
    """
    failures = []
    for path in sorted(DEMOS.glob("snippets/*.px")):
        code, out = run_px(praxis, path)
        want = path.with_suffix(".out").read_text()
        if code != 0 or out != want:
            failures.append(f"{path.name}: exit {code}\n--- got ---\n{out}--- want ---\n{want}")
            continue
        # The cards are a column, not a page. A line past this wraps or clips.
        wide = [n for n, l in enumerate(path.read_text().splitlines(), 1) if len(l) > SNIPPET_COLS]
        if wide:
            failures.append(f"{path.name}: line(s) {wide} are wider than {SNIPPET_COLS} columns")
            continue
        src = path.read_text().rstrip("\n")
        tag = f'<pre data-snippet="{path.stem}">'
        if tag not in html:
            failures.append(f"{path.name}: no {tag} on the page")
            continue
        i = html.index(tag) + len(tag)
        j = html.index("</pre>", i)
        html = html[:i] + esc(src) + html[j:]

        # The prose says what each one answers, so that comes from the run too.
        tag = f'<code data-out="{path.stem}">'
        if tag in html:
            i = html.index(tag) + len(tag)
            j = html.index("</code>", i)
            html = html[:i] + esc(want.strip().splitlines()[0]) + html[j:]
        print(f"  snippet  {path.stem:12} ok")

    # The parser vocabulary list is generated from the same files, so the
    # constructor spelled on the page is the one that was just run.
    items = []
    for path in sorted(DEMOS.glob("parsers/*.px")):
        code, out = run_px(praxis, path)
        want = path.with_suffix(".out").read_text()
        src = path.read_text()
        label = re.search(r"^//! (.+)$", src, re.M)
        shown = re.search(r"^//= (.+)$", src, re.M)
        if code != 0 or out != want:
            failures.append(f"{path.name}: exit {code}\n--- got ---\n{out}--- want ---\n{want}")
        elif not label or not shown:
            failures.append(f"{path.name}: needs a //! label and a //= parser expression")
        elif f"read {shown.group(1)}\n" not in src:
            failures.append(f"{path.name}: //= is not what the program reads")
        else:
            items.append(f"<li><b>{esc(shown.group(1))}</b> {esc(label.group(1))}</li>")
            print(f"  parser   {path.stem:12} ok")

    if failures:
        sys.exit("the page quotes code that does not run:\n\n" + "\n\n".join(failures))

    tag = '<ul class="plain" data-parsers>'
    i = html.index(tag) + len(tag)
    j = html.index("</ul>", i)
    return html[:i] + "\n        " + "\n        ".join(items) + "\n      " + html[j:]


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def book_stats(html):
    """Count the book rather than quoting a number that was true once."""
    book = ROOT / "docs" / "book"
    examples = list((book / "examples").rglob("*.px"))
    lines = (book / "src" / "SUMMARY.md").read_text().splitlines()
    # SUMMARY.md opens with its own `# Summary` title, so the part headings are
    # the `# ` lines after that one.
    stats = {
        "examples": str(len(examples)),
        "chapters": str(sum(1 for line in lines if line.startswith("- ["))),
        "parts": str(len([line for line in lines if line.startswith("# ")]) - 1),
    }
    for name, value in stats.items():
        # Every span that carries the stat, not the first one: a number the page
        # shows in two places is two chances to go stale, and the second is the
        # one nobody looks at.
        pat = re.compile(rf'(<span[^>]*\bdata-stat="{name}"[^>]*>)[^<]*(</span>)')
        html, hits = pat.subn(rf"\g<1>{value}\g<2>", html)
        if not hits:
            sys.exit(f'no <span data-stat="{name}"> on the page')
        print(f"  stat     {name:12} {value:>5}  in {hits}")
    return html


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--praxis", default=str(ROOT / "target" / "release" / "praxis"))
    args = ap.parse_args()
    praxis = str(pathlib.Path(args.praxis).resolve())
    if not pathlib.Path(praxis).exists():
        sys.exit(f"no praxis binary at {praxis} — run `just build --release` or pass --praxis")

    work = HERE / ".build"
    work.mkdir(exist_ok=True)

    caps = {}
    for name, argv in CAPTURES.items():
        p = subprocess.run([praxis, "--color", "always"] + argv, cwd=DEMOS, capture_output=True, text=True)
        caps[name] = {"out": p.stdout, "err": p.stderr, "code": p.returncode}
        print(f"  {name:10} exit {p.returncode}")
    (work / "captures.json").write_text(json.dumps(caps))

    keys = (HERE / "keys-tui.json").read_text()
    subprocess.run(
        [sys.executable, str(HERE / "record.py"), str(work / "tui.json"),
         f"{TUI_COLS}x{TUI_ROWS}", "--", praxis, "--color", "always"] + TUI_CMD,
        cwd=DEMOS, input=keys, text=True, check=True,
    )
    subprocess.run([sys.executable, str(HERE / "vt.py"), str(work / "tui.json"),
                    str(work / "tui-frames.json")], check=True)
    subprocess.run([sys.executable, str(HERE / "pack.py")], cwd=work, check=True,
                   env={"PYTHONPATH": str(HERE), **__import__("os").environ})

    blob = (work / "demo-data.json").read_text()
    if CLOSE in blob:
        sys.exit("the recorded data contains a closing script tag")

    html = PAGE.read_text()
    i = html.index(OPEN) + len(OPEN)
    j = html.index(CLOSE, i)
    html = html[:i] + blob + html[j:]
    html = check_snippets(praxis, html)
    html = book_stats(html)
    PAGE.write_text(html)
    print(f"inlined {len(blob)} bytes into {PAGE.relative_to(ROOT)}")


main()
