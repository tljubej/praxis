# The website

`index.html` is the whole page: one self-contained file with no build step, no
dependencies and no network requests. Open it directly, or serve the directory:

```sh
python3 -m http.server -d www 8000
```

That last property is why the mark in the header is inlined SVG in the markup and
the favicon is inlined as a `data:` URI rather than either one being a `<link>`
to a file. The source of truth for both is [`brand/`](../brand), which the page
does not load; changing the geometry there means changing it here by hand.

## What gets published

The **site** is that one file with the rendered book underneath it at `book/`.
`just site` assembles the two into `target/site` and `just site-serve` puts it
on `localhost:8000`; `.github/workflows/pages.yml` runs the same recipe and
uploads what it produced, so what is deployed is what you previewed.

Nothing else under `www/` is published — the rest is the machinery that produces
`index.html`.

**No hostname appears in the page.** The book is linked as `href="book/"`, which
is what makes a custom domain a DNS change rather than an edit. The cost is that
the book links are the one thing that does not resolve when `index.html` is
opened straight off disk; use `just site-serve` when that matters. See
[ADR-156](../docs/decisions/156-the-site-is-the-page-with-the-book-under-it.md).

## Nothing on it is transcribed

Every terminal, every code sample and every count on the page is produced by
running something, not by writing down what running it once printed.

```sh
just build --release   # or pass --praxis to point at another binary
python3 www/tools/capture.py
```

That one command does four things:

- runs the demo programs and inlines their real diagnostics and fault reports
  into the `<script id="demo-data">` block, along with the crash-debugger
  recording;
- runs each program under `tools/demos/snippets/`, diffs it against its `.out`,
  and **pastes its source into the matching `<pre data-snippet>`** — so the code
  the page shows is the code that just ran, in full rather than as an excerpt,
  and the prose's "answers `147`" comes from the same run;
- runs each one-liner under `tools/demos/parsers/` and generates the parser
  vocabulary list from them, checking that the constructor spelled on the page
  is the one the program reads;
- counts the book's chapters and examples and fills in the `data-stat` spans.

**A program that stops working stops the build.** `capture.py` exits nonzero
without writing the page, so a broken sample cannot ship. Adding a sample means
adding the `.px`, its `.out`, and a `<pre data-snippet="name">` for it.

Review the diff the way you would review `just book-bless` — it is the same
hazard, since a regression in the output would be recorded rather than caught.

The debugger recording is a **fresh session every time**, so the blob changes on
every run even when nothing else did: the frame boundaries follow how the pty
happened to flush and the timestamps follow how fast the machine was. Read that
diff for what the screens say, not for byte-equality, and do not re-run the
capture as part of an unrelated change.

## How the debugger recording is made

`praxis run --debug always` only draws the full-screen TUI when stdin and stdout
are a terminal; a pipe correctly takes the line-oriented prompt instead, and
`script(1)` reports a 0x0 window. So `tools/record.py` allocates a pty, sets its
size with `TIOCSWINSZ`, and feeds it the keystrokes in `tools/keys-tui.json` on a
schedule, logging the raw output with timestamps.

`tools/vt.py` then replays that byte stream through a small VT emulator — enough
of one for what ratatui emits — and writes one row-diff per redraw.
`tools/pack.py` interns the SGR attributes and produces the blob the page reads.
The player in `index.html` is the fourth piece: it re-renders those rows into
DOM, so the recording stays selectable text at any zoom rather than becoming a
video of a terminal.

Idle gaps are clamped to 0.72s on playback. Nothing else about the timing is
adjusted, and no frame is edited.

## Layout

```text
index.html              the page (the site is this plus the book at book/)
tools/capture.py        re-records, re-runs, re-counts, and inlines (start here)
tools/record.py         pty driver
tools/vt.py             VT emulator: raw stream -> row diffs
tools/pack.py           attribute interning and the final blob
tools/keys-tui.json     the keystrokes the recorded session presses
tools/demos/            the programs whose terminal output the page shows
tools/demos/snippets/   the programs the page quotes as code
tools/demos/parsers/    one parser constructor each, with the label the page shows
tools/.build/           scratch; gitignored
```

A file under `parsers/` carries two directives: `//!` is the human label and
`//=` is the parser expression shown on the page. The generator requires that
the `//=` text is literally what the program reads, so the two cannot drift.

Two of the demo programs — `tiles.px` and `maze.px` — are copies of book
examples, so if one of them changes under `docs/book/examples/` the copy here
needs the same edit. The rest are written for the site.
