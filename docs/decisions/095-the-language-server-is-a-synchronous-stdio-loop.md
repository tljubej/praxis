# ADR-095: The language server is a synchronous, single-threaded stdio loop

**Date:** 2026-08-01
**Status:** accepted
**Milestone:** 11 (language server MVP)

## Context

§15.1 says `praxis lsp` runs a JSON-RPC LSP process over stdio and maintains
open-document overlays, source revisions, parsed trees and caches. It does not
say how the process is scheduled, and the two live options — an async runtime
(`tower-lsp` on tokio) and a synchronous message loop (`lsp-server`, the
rust-analyzer transport) — differ in what they cost the rest of the workspace,
not in what they can serve.

Two facts decide it, and both are properties of this tree rather than opinions
about servers in general.

**The workspace has zero async dependencies.** Nothing in `Cargo.lock` at
`bcc5319` pulls tokio, futures or an executor. Adding one for a single crate is
a large dependency, a second scheduling model, and a `just ci` that builds it,
for a server whose entire working set is one file.

**`rowan::SyntaxNode` is `!Send`.** Only `GreenNode` crosses threads; a
`SyntaxNode` is a cursor into thread-local red-tree state. A multi-threaded
design therefore pays an immediate re-rooting cost on every worker to answer a
query about a tree it cannot share — the parallelism buys nothing at one-file
scope and costs a tree walk per request.

## Decision

`praxis-lsp` uses **`lsp-server` + `lsp-types`** — the rust-analyzer transport
crates — and runs one synchronous loop on the main thread. No async runtime.

- The loop owns the document store and the query cache outright. There is no
  lock, because there is no second thread that could want one.
- `lsp-server`'s `Connection::stdio()` spawns the two I/O threads that read and
  write framed messages; those threads move `Message` values and never a tree.
- `Connection::memory()` gives a test an in-process client with the same
  message type, so the protocol layer is exercised without a subprocess. The
  subprocess test still exists (WS1's gate) because the binary's own argv,
  framing and exit code are part of what M11 promises.

## Consequences

- **A slow query blocks the loop.** At one-file scope, with the whole front end
  measured in single-digit milliseconds for `praxis check`, this is the correct
  trade. §15.5's targets are about one AoC file.
- **`$/cancelRequest` drops the queued request; it does not kill a worker.**
  There is no worker. A request already being served runs to completion.
- **The query layer must never hand a `SyntaxNode` across a public boundary.**
  This is the constraint that keeps the later split cheap: if M13's measurements
  say one thread is not enough, the split is "a thread owns the `GreenNode` and
  re-roots per query", and that is a move rather than a rewrite only if no
  caller outside the crate is already holding a cursor. Public query results are
  owned data (types, ranges, strings) or `TextRange`s — never a node.
- The transport lives in its own module (`praxis-lsp::server`), separate from
  the queries (`praxis-lsp::query`), for the same reason: ADR-097 keeps a later
  crate split a move.
