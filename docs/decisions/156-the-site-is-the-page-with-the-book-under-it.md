# ADR-156: The site is the page with the book under it, deployed by the recipe you preview with

**Date:** 2026-08-19 · **Status:** accepted

## Context

The fifteen crates are on crates.io (ADR-155). Two documents were still only
readable from a checkout: `www/index.html`, which is a single self-contained
file, and the mdBook under `docs/book/`, whose render is gitignored because it
is derived. Someone who has just run `cargo install praxis-cli` has neither.

Three things had to be decided together.

**Where they go relative to each other.** They are one audience arriving from
one place, and the page is the thing that explains why the book is worth
opening, so the page is the root and the book is beneath it. The alternative —
two Pages sites, or a repository per document — buys separation nobody wanted
and costs a second deployment to keep in step.

**How the page names the book.** A full URL in the markup is a hostname
committed to the repository, and a hostname is the one part of a static site
that is likely to change: `tljubej.github.io/praxis/` today, a domain later.
Relative links have one cost, and it is real — `href="book/"` does not resolve
when `index.html` is opened straight off disk, which `www/README.md` documents
as a property of the file.

**What the page was still saying about installation.** The `start` section
predated the release and told a reader to install `just` and run the quality
gate. That is contributor instruction printed where a user is looking for the
one command that gets them a compiler.

## Decision

- **The published tree is `www/index.html` at the root with the rendered book at
  `book/`.** `just site` assembles it into `target/site`; `just site-serve`
  serves that. Only `index.html` is published from `www/` — the rest of the
  directory is the machinery that produces it.
- **No hostname appears in the page or the book.** The page links the book as
  `href="book/"`, so a custom domain is a DNS change and a repository setting,
  with nothing to edit. `just site-serve` is the documented way to preview the
  links.
- **`.github/workflows/pages.yml` runs `just site` and uploads what it
  produced**, per ADR-002 — the workflow has no build logic of its own, so the
  deploy cannot diverge from the local preview. It is `push` to `main` filtered
  to the paths that can change the site, plus `workflow_dispatch`.
- **mdBook is pinned to `^0.5`** in the workflow. `book.toml` uses the 0.5
  option names, and a docs deploy is a bad place to find out a renderer went to
  0.6.
- **The page's install card is `cargo install praxis-cli` and nothing else.**
  `just` is a contributor tool and the book's install chapter already frames it
  as one; the page names the crate and the binary, because they differ.
- **The page does not name a version at all.** It named one in three places —
  a header badge, the install card, the footer — none of them derived from
  anything, so every release was three chances to publish a page claiming the
  version before it. Teaching `scripts/set-version.sh` to move them was the
  other option and the worse one: the number tells a reader nothing they need,
  and not having it is cheaper than keeping it honest. `cargo install
  praxis-cli` resolves the current version without the page having an opinion.
- **A stat the page shows twice is generated in both places.** `capture.py`
  replaced only the first `data-stat` span, which was correct while every number
  appeared once and became a silent staleness bug the moment the book section
  repeated the chapter and example counts. It now rewrites every match and fails
  when a stat has no span at all.

## Consequences

Deploying needs Pages enabled on the repository with **GitHub Actions** as the
source — a one-time setting, and until it is set the build succeeds and the
deploy step fails. Pages on the free tier requires the repository to stay
public, which it is.

`workspace.package.homepage` now points at the site. crates.io reads that at
publish time, so the fifteen crates already on the registry keep pointing at the
repository until the next release carries the change.

The book gains no link back up to the site: mdBook's header links to the book
root, and a reader who lands on a chapter from a search engine has no way up.
That is a real gap and it is left open rather than solved by hand-editing the
theme.
