# Installing Praxis

Praxis is built from source. There are no binary releases, no package-manager
recipe and no installer: you need a checkout of the repository and a Rust
toolchain, and at the end of it you have a `praxis` binary that JIT-compiles
`.px` files.

## What you need

A stable Rust toolchain. The repository pins one in `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

`rustup` reads that file when you run any `cargo` command inside the checkout,
so it selects — and if necessary installs — the right toolchain for you. The
workspace declares `rust-version = "1.88"` as its minimum — the floor its own
dependencies impose — and anything newer on the stable channel is fine.

Nothing else is required to build the compiler. Praxis links Cranelift for code
generation, so there is no LLVM to install.

## Building the compiler

The `praxis` binary is the `praxis-cli` crate. From the repository root:

```console
$ cargo build --release -p praxis-cli
```

That produces `target/release/praxis`. Confirm it works:

```console
$ ./target/release/praxis --version
praxis 0.1.0
```

`cargo build -p praxis-cli` without `--release` writes `target/debug/praxis`
instead. The debug binary is a complete compiler and builds a good deal faster,
which matters while you are working on the compiler itself. Use the release
binary for anything you care about the running time of: the code generator and
the GC heap are ordinary Rust, and an unoptimized build of them is unoptimized.

To put `praxis` on your `PATH` — worth doing, because the VS Code extension
looks for it there by default:

```console
$ cargo install --path crates/praxis-cli
```

That builds in release mode and copies the binary into `~/.cargo/bin`.

## The `just` runner

The repository's quality gate is a [`just`](https://github.com/casey/just)
file. You do not need `just` to build or use the compiler, only to run the
checks the way CI runs them.

```console
$ cargo install just
```

The recipes are small, and `just` with no arguments lists them:

| recipe | what it does |
|---|---|
| `just build` | `cargo build --workspace` |
| `just test` | `cargo test --workspace` |
| `just clippy` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `just fmt` | `cargo fmt` — **modifies files** |
| `just fmt-check` | `cargo fmt --check` |
| `just ci` | `fmt-check`, then `clippy`, then `test` |
| `just asan` | the whole suite under AddressSanitizer, on a nightly toolchain |

`just ci` is the gate, and the point of it is that it is the *only* gate: the
hosted CI job checks out the tree, installs the toolchain, installs `just`, and
runs `just ci`. It has no logic of its own, so what CI does and what you do
before pushing cannot drift apart
([ADR-002](../../../decisions/002-ci-via-just.md)). `fmt` is deliberately not a
dependency of `ci` — CI verifies formatting, it never rewrites your files.

Two things about `just ci` are worth knowing before you run it the first time.

**It takes about seventeen minutes** on a development laptop, and most of that
is not compilation. On macOS the bulk is XProtect scanning each freshly linked
test binary the first time it is executed.

**Doctests are not part of it.** Every library crate sets `doctest = false`. An
example in a `///` comment is still compiled by `cargo doc`, but it is never
executed, so an assertion written there checks nothing — put it in a unit test.
The reason is cost, not principle: `rustdoc --test` has to analyze a whole crate
before it can discover that the crate has no doctests, that work is never
cached, and it was costing 95 seconds a run to execute the single doctest the
workspace had.

`just asan` needs a nightly toolchain (`rustup toolchain install nightly`) and
is not part of `ci`, because an instrumented build is a second full compile of
the workspace; hosted CI runs it on a nightly schedule instead. It does not
cover JIT-generated code — Cranelift emits that raw and no `-Z` flag reaches it
— so a green ASan run is necessary and not sufficient for a change that puts new
unsafe behaviour in generated code.

## The VS Code extension

The extension lives in `editors/vscode`. It is thin on purpose: it registers the
`.px` extension, launches `praxis lsp`, contributes four commands and a TextMate
grammar for highlighting before the server attaches. There is no parsing and no
type logic in it. Everything the editor knows about your program arrives from
the compiler over the protocol, so the two cannot disagree about what a file
means.

Build it, then package it:

```console
$ cd editors/vscode
$ npm install
$ npm run compile
$ npx @vscode/vsce package
```

That writes `praxis-0.1.0.vsix` in the same directory. Install it from the
Extensions view's `…` menu → **Install from VSIX…**, or from a shell:

```console
$ code --install-extension editors/vscode/praxis-0.1.0.vsix
```

The `.vsix` is gitignored, and reinstalling the same version replaces the
previous one.

Then point the extension at your binary. The setting is `praxis.binaryPath`; it
defaults to the bare name `praxis`, resolved on `PATH`. If you did not run
`cargo install`, set it to an absolute path to `target/release/praxis`. That one
path is used for the language server *and* for the run/check/watch commands, so
there is a single thing to get right; changing it restarts the server.

If the server cannot start, you get an error message naming the command it tried
and the setting to change, rather than a stack trace. It is nearly always a
`praxis.binaryPath` that points at nothing.

The four commands are `Praxis: Run File`, `Praxis: Check File`, `Praxis: Watch
File` and `Praxis: Restart Language Server`. The first three save the buffer and
then run the binary in an integrated terminal — a terminal rather than an output
channel, because [the crash debugger](../debugger/entering.md) is interactive
and a write-only pane cannot answer a prompt. `Run File` appends `--input
input.txt` when a file by that name sits beside the source. `Watch File` runs
`praxis watch`, which is not implemented — its palette title says so in as many
words — and the command exists so that the binary's own message is what you see
rather than nothing at all.

Everything else arrives without the extension contributing anything, because it
is a server capability: diagnostics, hover, completion, signature help,
go-to-definition, document symbols, semantic tokens, find references, rename,
workspace symbols, inlay hints and quick fixes. See [Editor
support](../tooling/editors.md) for what each of them does.

Two defaults to know about. **Inlay hints are on**, so an unannotated binding or
parameter shows the type the compiler inferred, and `?T` where inference has not
pinned one; accepting a hint writes the annotation into the file wherever that
is legal. **Formatting is not implemented** and the server does not advertise
it, so `Format Document` does whatever VS Code would do unaided.

## Other editors

Any LSP client can drive `praxis lsp` — it speaks JSON-RPC over stdio and takes
no arguments. Clients that append `--stdio` to the server's argv are fine: the
flag is accepted and ignored, because stdio is the only transport there is.

## Running the book's examples

Every program in this book that is shown together with its output is a real file
under `docs/book/examples/`, and `docs/book/examples/verify.sh` re-runs all of
them against the expectations printed here:

```console
$ ./docs/book/examples/verify.sh getting-started

6 ok, 0 failed
```

If a chapter and the compiler ever disagree, that script is what says so.
