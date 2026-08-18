# The compiler pipeline

`praxis run foo.px` is one process that does everything: it lexes, parses,
resolves names, infers types, lowers twice, generates machine code with
Cranelift, and calls the result. There is no object file, no linker and no cache
on disk. A program that compiles at all compiles in a few milliseconds, which is
why the design never bothered with separate compilation.

This chapter walks that path stage by stage — what each stage consumes, what it
produces, and which crate owns it. It is for someone who wants to change the
compiler, or who wants to know where a particular behaviour is decided.

## The stages

| stage | crate | what comes out |
|---|---|---|
| lex | `praxis-parser` (`lex.rs`) | a token stream including trivia, plus `T0xx` |
| parse | `praxis-parser` (`parse.rs`) | a `rowan` green/red tree, plus `P0xx` |
| typed AST | `praxis-ast` | typed wrappers over syntax nodes — nothing copied |
| resolve | `praxis-hir` (`resolve.rs`) | a scope tree, a `SymbolId` per declaration, plus `N0xx` |
| infer | `praxis-hir` (`infer.rs`), `praxis-typeck` | a type per expression node, plus `Y0xx` |
| coverage | `praxis-hir` (`exhaustive.rs`) | match exhaustiveness and reachability |
| typed HIR | `praxis-hir` (`lower.rs`) | a `TypedModule`: every node carries a `Type` |
| monomorphize | `praxis-hir` (`mono.rs`) | one clone of each generic function per concrete use |
| MIR | `praxis-mir` (`build.rs`) | basic blocks over slots, with safepoints and fault edges |
| liveness | `praxis-mir` (`liveness.rs`) | the root set and the debugger's set at each safepoint |
| verify | `praxis-mir` (`verify.rs`) | a refusal, if the MIR broke an invariant |
| codegen | `praxis-codegen-cranelift` | finalized machine code in memory |
| run | `praxis-runtime` | the heap, the collections, the faults |

`praxis check` stops after coverage. Everything below that line is `praxis run`
only, which is why the editor can never be slowed down by the back end: the
language server's manifest does not depend on `praxis-mir`,
`praxis-codegen-cranelift` or `praxis-runtime`, and a test reads the manifest and
says so rather than observing that one code path happened not to reach the JIT.

## Source, tokens, tree

`praxis-source` is the leaf of the workspace and depends on no other Praxis
crate. It owns files, byte spans, the line map, and the `Diagnostic` type
together with its rendering. Every diagnostic carries a category letter and a
number — `T` for token, `P` for parse, `N` for name, `Y` for type, `I` for the
input parser — and the whole allocation is listed in
[the diagnostic index](../tooling/diagnostics.md).

The lexer is hand-written and emits trivia as tokens rather than discarding it. A
backtick template is lexed as **one** token, interior and all; the interior is
re-scanned later by a different crate, and the two agree about where nested
templates end because they share `praxis-syntax`'s template module.

The parser is recursive descent for statements with a Pratt loop for operators,
emitting into a `rowan::GreenNodeBuilder`. The tree is lossless:
`node.to_string()` reproduces the source byte for byte. On an
unexpected token the parser reports, wraps the stray token in an error node,
resynchronizes and continues, so one bad file still yields the rest of its
diagnostics.

`praxis-ast` is a thin typed layer over that tree — `SourceFile`, `FnItem`,
`ParserExpr` and friends — with no strings copied out of the source.

## Names and types

`praxis_hir::analyze` runs three passes and returns one `Analysis`:

```rust
let resolution = resolve::resolve(file, root);
let mut inference = infer::infer_with_tree(file, resolution, root);
exhaustive::check_matches(/* … */);
```

Resolution builds the lexical scope tree and mints a distinct `SymbolId` for
every declaration, which is what makes shadowing work: two `var x` in one block
are two symbols, and every downstream consumer — go-to-definition, rename, the
inlay hints — keys on the symbol and never on the spelling.

Inference is one file of about 3,800 lines (`infer.rs`) and it is where the
interesting decisions live; see
[what inference does](../types/model.md). `praxis-typeck` is the machinery it
drives rather than the algorithm itself: the interned type arena (`db.rs`),
unification, generalization by binding level, capability constraints, and
`pretty.rs`, which is the single place that decides how a type prints.

Match coverage runs **last**, after the whole file is inferred, because a
scrutinee's type is not final until then. That ordering is what puts a
non-exhaustive `match` in front of `praxis check` and the editor rather than only
in front of `praxis run`.

`Analysis` is the front end's whole output: the type arena, the symbol table, the
scope tree, per-reference and per-node types, resolved method calls, the retained
parser indexes, and every diagnostic.

## Typed HIR

The back end needs the type of *every* node, not just of name references, and it
must not re-run unification to get one. So a separate pass reads the finished
`Analysis` and rebuilds the program as a typed tree — `TypedModule`, `TypedItem`,
`TypedStmt`, `TypedExpr` — where each node carries an interned `Type` handle.
That tree is the boundary between the front end and the back end, and it never
unifies: it reads what inference recorded.

Typed HIR is still structured: `if`, `while`, `for`, `loop`, `match` and closures
are all nodes. What it removes is name lookup and method resolution — a
`MethodCall` node carries the catalog row's runtime symbol where the row is an
intrinsic — and it wraps a file's top-level statements in a function of their
own.

That function is called `<entry>`, and a crash backtrace names it:

```praxis
// The top-level statements of a file are a function the compiler wrote, and
// the crash backtrace names it `<entry>`. The temps under it are MIR locals.
fn half(n: Int) -> Int {
    return n / 0
}

var xs = [4, 8]
out(half(xs[0]))
```

```text
error: program faulted: division by zero

Backtrace:
#0   half
#1   <entry>

  locals:
    n: Int = 4
  temps:
    <tmp#2: Int> @ "0" = 0
    <tmp#3: Int> @ "n / 0" = <uninit>
    <tmp#4: Unit> @ "return n / 0" = <uninit>
```

The three `<tmp#N>` lines are MIR slots, not source variables: lowering
materializes every intermediate node of an expression tree into its own local,
and the debugger prints each with the expression that produced it. `<entry>` is
the generated function holding the file's top-level statements, and it is the
only entry point there is.

Monomorphization sits between typed HIR and MIR: each polymorphic function is
cloned once per distinct set of concrete type arguments at its call sites, so MIR
never sees a type variable. You never write a call's type arguments, and there is
no syntax to: brackets after a name are type arguments only where the name is a
type, so writing `id[Int]` on a function is reported as a subscript — `Y020`.

## MIR

MIR is a control-flow graph over **slots**, and it is deliberately not SSA:
Cranelift builds the SSA a stage later, and duplicating that work here would buy
nothing. A function is a list of `Local`s plus a list of `Block`s; a block is
instructions and a terminator. Every local is one of two kinds:

- `LocalKind::Gc` — holds a uniform `GcRef`. These are the only locals the
  collector ever sees.
- `LocalKind::Scalar` — a transient `i64`/`f64`/`u32`/`u8`/`bool` payload pulled
  out of an object for a local computation. It must not survive a safepoint; the builder
  materializes a fresh `GcRef` before any call, store or return.

So `a + b` lowers to `ExtractScalar`, `ExtractScalar`, `IntBinOp`, `CheckFault`,
`Materialize` — and a chain of arithmetic emits a `Materialize` immediately
followed by an `ExtractScalar` of the same value at every interior node. A
block-local forwarding pass deletes those cancelling pairs before anything else
runs, because deleting a `Materialize` deletes a safepoint, and liveness must
see the safepoints that survive rather than the ones the builder emitted.

This is also where a pipeline chain becomes one loop. The builder recognizes
`v.map(f).filter(p).sum()` on the typed tree it was handed and emits a single
fused loop over the source with no intermediate collection, and there is no
second, per-combinator lowerer behind it — a chain the recognizer declines is a
compiler bug that says so, not a silently wrong answer.

Then `annotate` runs backward-dataflow liveness and records, at every safepoint,
two sets: what the **collector** must keep alive, and what the **debugger** must
be able to render. They are deliberately different — the first is minimal so that
dead values are collectable, the second is over-approximate so that a crash can
still show you a local the program has finished with.

Finally `verify` checks the invariants: no scalar live across a safepoint, no
`ExtractScalar` whose width contradicts what the slot provably holds, every
safepoint annotated. A verifier failure is reported as an internal error and no
code is generated from it. It is never a program error.

## Cranelift

`praxis-codegen-cranelift` maps each MIR `Local` to a Cranelift `Variable` and
lets Cranelift's builder construct SSA, including the block parameters for loop
backedges. Every generated function has the same signature:

```text
fn(RuntimeContext*, GcRef...) -> GcRef
```

`GcRef` is a pointer, and Cranelift carries it — and every scalar payload — as
`i64`. The JIT refuses to initialize on a target whose pointer is not 64 bits or
whose endianness is not little, because those are host assumptions written as
constants rather than derived from the ISA.

Runtime calls resolve through one manifest. `praxis-stdlib`'s `abi.rs` has one
row per `praxis_*` symbol — 184 of them — giving the exact linker name, the
parameter and return kinds, and whether the wrapper can allocate, can fault, both
or neither. That last column is what MIR consults to decide whether a call site is
a safepoint and whether a fault check follows it, so a wrapper's effect is a fact
in a table rather than a property of the instruction shape. Every wrapper in that
table is `extern "C"`, never panics, and reports a problem by setting a pending
fault rather than by returning one.

Not everything is a call. The backend compiles at Cranelift's
`opt_level = "speed"`, and several hot operations are inlined branches: a scalar
load proves the object's type with one compare, a small `Int` comes from an
interned table behind the pacing test, and generated code claims an allocation
block inline. The proofs are branches rather than calls because the branch
predicts and the call does not; none of them is elided, because a wrong one is
a memory-safety bug and not a slow path.

Everything the backend mints for the runtime to read by raw pointer — record and
tuple schemas, field names, debug metadata — belongs to a `Generation`, an arena
with interning. Reclaiming one requires proof that the heap has been drained,
because live objects point into it; a generation that is merely dropped leaks on
purpose.

## The `read` sub-pipeline

A `read` or `parse` expression is a second small compiler running beside the
first, and it finishes at `praxis check` time — a parser that does not check is a
compile error, not a runtime one.

The ordinary parser produces `PARSER_EXPR` nodes and one opaque
`BacktickTemplate` token. `praxis-hir`'s `parser_lower.rs` converts those nodes
into `praxis-input-parser`'s own AST, and that crate does the rest: `scan.rs`
re-scans a template's interior into literal runs and captures, `body.rs` parses a
capture's body as a full parser expression, `validate.rs` and `call.rs` check the
shape before anything is built, `synthesize.rs` computes the result type — which
is where a `Vec[Int]` comes from when you never wrote one — and `plan.rs` lowers
it to a `ParserPlan` registered under a `PlanId`.

MIR carries that `PlanId` as an immediate. At run time `praxis-runtime`'s
`parser.rs` interprets the plan against the input buffer. The plan is
interpreted, not compiled, and that is a current implementation choice rather
than a property of the design. See
[how a parser gets its type](../input/type-derivation.md).

## The shared query layer

The CLI and the language server run the same front end through one query API. It
lives in `praxis-lsp` — the crate that needs it most — with `praxis-cli`
depending on `praxis-lsp` rather than the other way round. Two front ends would
be two places to teach every new rule, and `praxis check` and the editor would
be one forgotten edit away from disagreeing about the same file.

`query::Snapshot` is one file at one revision with the front end memoized on it:
`parse` and `analyze` each run at most once per snapshot, and a test asserts the
run counts rather than assuming them. `diagnostics()` is the one place that
decides which diagnostics exist and in what order — parse first by construction,
then names and types, all sorted by span — and the one place that decides analysis
runs even when parsing reported, because recovery keeps the tree usable and an
editor must not go blank on one stray character.

The whole of `praxis check` is then:

```rust
let snapshot = Snapshot::new(file, text, Revision(0));
let diagnostics = snapshot.diagnostics();
```

so a divergence between what `praxis check` prints and what the editor underlines
is unrepresentable rather than merely unlikely.

`praxis run` does **not** route through the snapshot. It calls
`praxis_parser::parse` and `praxis_hir::analyze_root` directly, because it needs
the `Analysis` by value to hand to lowering and then to the crash debugger, and it
re-states the sort. That is the one place the sequence is written twice.

A `rowan::SyntaxNode` never leaves the query layer. It is `!Send` and it is a
cursor into thread-local state, so `Snapshot::parse` is crate-private and every
public answer is owned data or a range. The server itself is a synchronous,
single-threaded stdio loop with no async runtime, and keeping syntax nodes
crate-private is what makes moving the front end onto its own thread a move
rather than a rewrite.

## Reading what the back end emitted

Three environment variables dump the compiler's own output, on stderr, from the
real compile path. Each takes `1`/`all` or a comma-separated list of function
names.

| variable | what it prints |
|---|---|
| `PRAXIS_DUMP_CLIF` | the Cranelift IR, post-optimization, with an instruction count per block |
| `PRAXIS_DUMP_VCODE` | the machine-level listing, same header |
| `PRAXIS_DUMP_SLOTS` | one census line per function |

```console
$ PRAXIS_DUMP_SLOTS=all praxis run references-are-copied.px
;; praxis-dump slots `push_two`: gcloc=6 rootc=2 live=2 dbgvis=5 nameless=1 unrenderable=0
;; praxis-dump slots `rebind`: gcloc=11 rootc=2 live=2 dbgvis=10 nameless=4 unrenderable=0
;; praxis-dump slots `<entry>`: gcloc=10 rootc=2 live=2 dbgvis=7 nameless=2 unrenderable=0
[1, 2]
[1, 2]
[1, 2]
```

`gcloc` is the function's count of `Gc` locals, `rootc` the shadow stack's claim
width — the colours the interference relation needs — `live` the largest root set
live at any one safepoint, which equals `rootc` wherever the colouring is optimal,
and `dbgvis` the largest set the crash debugger must be able to render. The gap
between `gcloc` and `rootc` is the colouring: a shadow slot is a live range and
not a name, so locals that are never live at the same safepoint share one.

These hooks are in the tree permanently, because an instruction count is a
deterministic result for a change that removes three instructions from a loop, and
a wall clock is not.

## The crate graph

Sixteen crates. The stage table at the top of this chapter names the ones that
are a stage, and `praxis-source` is the leaf underneath all of them. Five more
are worth knowing by name:

- **`praxis-syntax`** owns the `SyntaxKind` vocabulary, the identifier character
  class, and the one rule for where a template or an interpolated literal ends —
  which is why the lexer and the input parser agree about where a nested
  template closes instead of each having an opinion.
- **`praxis-repr`** holds the one total, bidirectional bridge between a static
  `Type` and a runtime `TypeDescriptor`. The two directions have to be inverses,
  and two independently written halves are each locally plausible while failing
  to compose, so they live in one module with an exhaustive match on each side:
  a new built-in type is a compile error there until both directions know it.
- **`praxis-stdlib`** is the single source of truth for what built-in methods
  exist, what their types are, and how they lower — consumed by inference,
  lowering, codegen and the language server's completion alike, so that method
  knowledge is never written down twice.
- **`praxis-debugger`** is the crash debugger: the snapshot, the command loop,
  the read-only expression evaluator and the full-screen view.
- **`praxis-lsp`** is LSP transport *and* the shared query layer above, with the
  CLI as its consumer.

One direction in that graph is worth stating outright. `praxis-input-parser`
depends on neither `praxis-parser` nor `praxis-hir`: the ordinary lexer hands it
a template as a single token and the ordinary parser hands it the
parser-expression nodes, so the second compiler needs no lexer of its own and
cannot drift from the first about what a token is.
