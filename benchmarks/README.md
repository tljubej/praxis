# Praxis benchmark suite

Eight benchmarks, each written three times — once in Praxis, once in Rust, once
in Python — against the same algorithm, the same data structures and the same
checksum. `run.py` refuses to time a benchmark whose three implementations do
not print byte-identical output, so a fast number can never come from a program
that quietly did less work.

The generated report is [`REPORT.md`](./REPORT.md); the raw measurements are in
`results.json`.

## Running it

```bash
cargo build --release && python3 benchmarks/run.py && python3 benchmarks/report.py
```

`run.py` measures and writes `results.json`; `report.py` renders `REPORT.md`
from it, so the report can be regenerated without re-running the suite and
every number in it is read rather than transcribed.

### The knobs

`--pilot` runs every benchmark at a tiny size (correctness only, a few seconds
end to end), `--only primes,vm` selects a subset, and `--reps N` sets the
repetition count of the timing pass. Rust binaries are compiled on demand into
`benchmarks/.build/` and rebuilt when their source is newer.

The workload sizes in `sizes.json` were chosen so Praxis peaked near 6 GiB of
resident set, back when memory rather than time was what bounded how large they
could be. Since the collector's threshold gained a ceiling
([ADR-112](../docs/decisions/112-the-pacer-has-a-ceiling-and-the-live-set-may-exceed-it.md))
that is no longer true and the sizes could be raised substantially. They have
deliberately **not** been re-tuned: changing a size moves a benchmark to a
different rung of the pacer's power-of-two ladder, which is exactly what the
appendix measures, so the two changes must not be made in one step.

### The pacer A/B

`pacer_ab.py` runs the appendix's experiment. Both pacing rules ship in the same
binary behind `PRAXIS_GC_PACER`, so the two arms are one executable run twice —
no second target directory and no second link. It writes `gcfix.json`, which
`report.py` renders the appendix from if the file is there.

```bash
cargo build --release && python3 benchmarks/pacer_ab.py --arm-b bounded:64M:2
```

**`gcfix.json` carries both arms**, measured against each other in one
interleaved session. That is what makes it self-contained: the old one-arm
schema had to be compared against whatever `results.json` happened to hold,
which might be a completely different build. `report.py` skips the appendix for
a file without an `arm_a` key rather than render it against the wrong control.

The other file in the directory, `gcfix-pre-perf-fixes.json`, is that old
one-arm schema and is deliberately named so `report.py` does not find it. It was
measured against the build at `fd70374`, before the six findings in
[`../docs/handovers/21-where-the-time-goes.md`](../docs/handovers/21-where-the-time-goes.md)
were fixed. It is kept because it is the measurement ADR-112 overturns, and it
should not be renamed: there is nothing left in the tree it could be a control
for.

`run.py` **refuses to run** with `PRAXIS_GC_PACER` set. The variable changes the
collector's schedule and nothing else, so a stale export would move every Praxis
number in `results.json` without moving one character of its output — and
`REPORT.md` would then describe a collector this workspace does not ship.

Neither `.build/` nor `results.json` is part of `just ci`: this directory is
outside the cargo workspace and nothing in it is compiled by `cargo build
--workspace`. That is deliberate — the Rust benchmarks are single files built
with `rustc` directly, so they cannot drift into the workspace's lint and
formatting gates.

## What each one measures

| Benchmark | What it stresses |
|---|---|
| `primes` | Scalar `Int` arithmetic and call overhead. Trial division; no collections, no allocation the program can see. |
| `mandelbrot` | `Float` arithmetic. Escape-time iteration on binary64, with a data-dependent inner trip count. |
| `collatz` | Unpredictable branches. Every step branches on the parity of a number just derived from an unpredictable one. |
| `vm` | Dispatch. A ten-opcode stack bytecode interpreter: one `match` per retired instruction, an operand stack in a `Deque`. |
| `hashwork` | Hash collections. `Map` inserts, `Set` inserts, `Counter` increments, then a lookup pass that misses about a third of the time. |
| `tree` | Records and recursion. A 131071-node tree in an arena, walked recursively once per rep. |
| `pipeline` | The §6.3 sequence combinators — `map`/`filter`/`sum` and `enumerate`/`map`/`fold` — against Rust iterators and Python generators. |
| `bfs` | Traversal. Level-synchronous breadth-first search over a 320×320 grid's adjacency lists. |

## How the three implementations are kept comparable

**Same algorithm, same structures, each spelled idiomatically.** Where a
language's natural choice for a role differs, each gets its own: the dense
visited mark in `bfs` is a Praxis `BitSet`, a Rust `Vec<bool>` and a Python
`bytearray`; a tree node is a Praxis record, a Rust struct and a Python tuple.
Where the structures are the same thing, they are the same thing: `hashwork`
uses Rust's `std::collections::HashMap` with its default SipHash hasher because
that is literally what the Praxis runtime's `Map` is built on
(`crates/praxis-runtime/src/maps.rs`), and swapping in a faster hasher would be
measuring a different program.

**No constant folding of the workload.** Every program reads its workload size
from stdin, so no compiler can precompute the answer and no `black_box` is
needed to stop it.

**No arithmetic that the three disagree about.** Praxis's `%` truncates like
Rust's and Python's floors, so every remainder in the suite is taken on
non-negative operands; every accumulator is reduced mod 1000003 or otherwise
bounded so no intermediate approaches 2^63, where Praxis would fault, Rust would
wrap and Python would silently promote to a bignum.

**Praxis constraints that shaped the code.** `Vec` has no element assignment and
no `pop`, so mutable indexed state is a `Grid`, a `Map`, or — in `vm` — a
`Deque` used as a stack. There are no self-referring types, so `tree` uses an
arena of records linked by index rather than a boxed tree; the other two
implementations keep that shape rather than switching to a structure Praxis
cannot express.
