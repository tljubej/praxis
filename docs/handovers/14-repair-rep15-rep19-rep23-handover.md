# Repair session handover — the last P0, and §3.3 verbatim

**Date:** 2026-07-30
**Tree:** `3678b6d` · **Suite:** 1443 passed, 0 failed, 38 ignored · `just ci` green

`implementation-repair-progress.md` is still the living status and this file does
not replace it — read §1 and §4 there first. This is the session-scoped note: what
landed, where to start, and the things that would otherwise have to be
rediscovered.

## What landed

| Row | Commit | ADR |
|---|---|---|
| **REP-15** (P0) — a `for` iterates a snapshot | `a1c0b76` | **066** |
| **REP-19** (P1) — a file's top-level statements are its program | `11e107c` | **067** |
| **REP-23** (P1) — a fused pair carries both of its halves | `3678b6d` | 066 (decision 5) |

**The repair's last P0 is closed**, and **§3.3's representative program now runs
verbatim from the design doc** — top-level `let`, top-level `out`s, no `fn main`
anywhere. `tests/aoc-corpus/s33_representative_program.px` *is* the design doc's
text; the `fn main()` wrapper and the comment explaining it are gone.

**Two findings registered: REP-22 and REP-23.** REP-23 landed with the session.
**REP-22 is a new P0** and is where to start.

## Where to start

**REP-22** — a `fn` body that reads a top-level binding.

```praxis
let x = 1
fn f() { x }
out(f())          // Unit

let y = 5
fn g() { |n| n + y }
out(g()(1))       // 4388746929
```

Both pass `praxis check`. Resolution resolves the name to the top-level symbol and
inference types it; MIR has no slot for it inside the function. A `fn` does not
capture (§4.9/§4.10 — closures do, functions do not) and **nothing says so**.

**This session did not cause it.** Both forms were measured at `a1c0b76`, before
REP-19, and behave identically there. What changed is reachability: the design
doc's program shape puts bindings at the top level, and REP-19 made that shape
work. A **top-level** closure is fine — `let offset = 10` then
`v.map(|x| x + offset).sum()` answers `23`, because both live in `<entry>` — so
the boundary is a `fn` body, not a closure body.

The decision: **report it** (a name mistake; ADR-051 puts it in the `N0xx` block
and **`N007` is next free**) **or make top-level bindings globals**, which §3.2
does not decide and which needs storage, initialization order and a GC root. The
narrow defect that survives either answer is the **silence** — REP-14's shape.

If reporting: a `fn` body's scope is a child of the enclosing scope, so the lookup
walks straight out. What is missing is the *boundary* — `ScopeTree::lookup` does
not report which scope it found the binding in, and `resolve_fn` does not mark its
body scope as a function boundary. Both are small. The care is in not reporting a
closure's legitimate capture, and in what a `fn` naming another `fn` (or a
`struct`, or a builtin) must keep doing.

The other two rows, both below P0: **REP-10** (P2, S25's last scheduled row —
record and tuple patterns, and the other half of `for (k, v) in m`) and **REP-21**
(P3, `min=`/`max=`, whose runtime wrappers and catalog row names already exist).

## Six things worth not rediscovering

1. **An nth-member accessor is the wrong protocol, and it is the one the code's
   shape suggests.** `get_symbol_for`/`len_symbol_for` looked like they wanted one
   more match arm each. They cannot have one: a `HashSet` has no nth member, so
   every loop over a hashed collection would be quadratic, and a `BinaryHeap`'s
   backing array is heap-ordered only at its root, so indexing it answers by
   insertion history — a *different* wrong answer, not a fix. The snapshot is one
   call per loop and it is also what makes mutation-during-iteration well-defined.
   `IterPlan` has **no default arm**; the `_ => VecGet` default *was* REP-15.

2. **`Map` and `Counter` needed no new wrapper, and the reason is not economy.**
   REP-18's `keys()`/`values()` are index-aligned because they share one order, so
   they already are the protocol. The pair is built in **MIR** because a `Map`'s
   payload records `INT` as its value descriptor unconditionally (there is a
   comment saying so in `praxis_map_new`) — a runtime-built pair for a
   `Map[Text, Text]` would read its value as an `i64`. The compiler's item type is
   right where the payload's is not.

3. **A `TupleSchema` slot can be null now, and two separate things needed it.**
   `let m = Map()` plus a `for kv in m` that never opens the pair leaves K and V
   unresolved, and requiring every slot to resolve *rejected that program* — the
   first thing that failed when the REP-15 gates were written. The same null slot
   then fixed REP-23, where an `Opaque` tuple type degraded to a **zero-arity**
   schema and `praxis_alloc_tuple` sizes the payload from the schema, so both
   `praxis_tuple_set` calls wrote into nothing. The runtime reads the value's own
   descriptor off its header for a null slot, which is never wrong.

4. **`an_opaque_tuple_type_yields_an_empty_schema` was a bug-pinning test and
   §8.2 did not list it.** It asserted the empty schema that dropped every fused
   pair's elements, so a correct fix made it red as a "regression". §8.2's table
   of five is not exhaustive — a passing test that asserts a defect can be
   anywhere, and the tell is a test whose assertion *is* the finding's
   reproduction.

5. **§3.2 already required REP-19's fix**: "A single file is the normal program
   unit. Top-level statements are wrapped in a generated entry function." The only
   thing actually open was what happens when the file *also* declares `fn main`,
   and the answer is a **fallback** rather than a layer — running the top level
   and *then* calling `main` runs `main` twice for `fn main(){…}` + `main()`, and
   reporting a file with both rejects that ordinary program. Twelve corpus
   programs are `fn main` with nothing at top level, so the fallback is what let
   the suite go green-to-green.

6. **`<entry>` is not an identifier, on purpose** — ADR-064's rule for `[]`/`[]=`
   at the one other name the compiler mints into that namespace. A gate asserts
   the property rather than trusting it, and the crash debugger renders the name,
   so a fault in a top-level statement shows a frame the user can tell is not
   theirs.

## The `praxis check` sweep

Done, over `crates/praxis-cli/tests/fixtures`: `bad_byte.px`, `parse_error.px` and
`type_error.px` fail, and they are the three intentional negative fixtures. The
`tests/` corpus is executed by
`every_corpus_program_runs_and_prints_the_answer_it_documents` (REP-12) and gained
two programs this session: `rep15_iterating_every_collection.px` (one generic
`digits(c)` walking six collections) and `rep19_top_level_program.px` (§3.2's
shape). `s33_representative_program.px` was rewritten to the design doc's text.

## One thing noticed and not chased

`read grid(int)` over `12\n34\n` produces a 2×2 grid whose cells are
`[12, 2, 34, 4]`. `a_grid_subscript_takes_both_coordinates_in_the_order_the_design_names`
already documents it (it asserts `g[0,0]` is `12`), so it is not new, and it is the
input parser's rather than iteration's — a `for` over that grid visits exactly the
four cells `g.cells()` reports. It is not in the register because it was not
reproduced against a stated contract; §7's grid parser is where that contract
would come from.
