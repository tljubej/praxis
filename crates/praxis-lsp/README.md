# praxis-lsp

The Language Server Protocol implementation for
[Praxis](https://github.com/tljubej/praxis).

Two halves that do not depend on each other:

- **`query`** — the shared front-end query API. `praxis check` routes through
  it, so a divergence between what the CLI prints and what the editor underlines
  is unrepresentable rather than merely unlikely.
- **`server`** — JSON-RPC over stdio, one synchronous loop, no async runtime.

Everything between them is a query: diagnostics, hover, completion, signature
help, go-to-definition, references, rename, document and workspace symbols,
semantic tokens, inlay hints and code actions. Inlay hints show the types you did
not write, so `fn foo(a, b)` reads as `fn foo(a: Int, b: Int)`.

The `read` sublanguage gets the same treatment — hover on an inner constructor,
capture-type completion, its own token classes — by reading the index inference
already retains. There is no second scanner over template interiors here, on
purpose.

## Three rules the editor features are built on

- **A quick fix is a diagnostic's machine-applicable suggestion.** The code
  action layer knows about no particular diagnostic; the fix for a misspelled
  constructor is written where the constructor table is consulted.
- **A rename is safe when re-resolution is unchanged.** Rename analyzes the
  edited text and compares, rather than enumerating collision kinds against a
  scope tree that cannot answer where an offset is.
- **A name's identity is its symbol, never its spelling** — which is what tells
  two shadowed bindings apart.

## What it must not reach

Editing a file updates diagnostics without running JIT code, and that holds by
construction: this crate's manifest does not depend on `praxis-mir`,
`praxis-codegen-cranelift` or `praxis-runtime`, and a test reads the manifest
and says so. Observation would only have proved that one code path did not reach
the JIT today.

## Running it

The server ships inside the `praxis` binary:

```console
$ praxis lsp
```

The repository has a VS Code extension under `editors/vscode/` that launches it.

## Part of Praxis

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzles: the input parser is part of the language, types are inferred
rather than written, and a program that falls over hands you its state instead
of a stack trace.

To *use* the language, install [`praxis-cli`](https://crates.io/crates/praxis-cli)
— it provides the `praxis` binary. The
[repository](https://github.com/tljubej/praxis) has the book, the design
document and the decision records.

This crate is one stage of that compiler, published so the pipeline is
inspectable and so `praxis-cli` can be built from the registry. Its API tracks
what the compiler needs and is not a stable platform for outside consumers.

Praxis was written with large language models against a human design. The
repository's README says what that means for the license.

Licensed under either of Apache License 2.0 or the MIT license, at your option.
