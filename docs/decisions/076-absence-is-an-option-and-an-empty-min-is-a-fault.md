# ADR-076: Absence is an `Option`, and an empty `min` is a fault

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S18 — RT-14, RT-15, D1)

## Context

D1 is the decision the whole repair was blocked on. It was answered in the plan
(§7, commit 497ef35) and this records it as implemented, with the reasoning that
survived contact with the code.

```praxis
let v = m.get(k)   // statically V, actually the Unit sentinel when absent
let p = g.find(x)  // statically (Int, Int), same
```

Both wrappers were declared non-faulting and both answered
`unsafe { unit_sentinel(ctx) }` on a miss. A program therefore held a value whose
static type was `Int` and whose runtime descriptor was `Unit`, from three lines
of source, with nothing in the workspace able to notice.

The design document had already decided this and the implementation had never
followed. §5.7 writes the signature literally — `Map[K,V].get(K) -> Option[V]` —
and §4.7 opens "Option[T] represents normal domain-level absence. It is not an
error channel", shows `match map.get(key) { Some(value) => … None => … }` as its
worked example, and reserves faulting for `map[key]`: "the user chooses between
explicit absence with `.get` and assertion-like access with indexing." Both
halves of that sentence already existed as separate catalog rows (ADR-064's
`INDEX_READ`, and `praxis_map_index` raising `IndexOutOfBounds`), so keeping
V-with-Unit would have left the language with the fault half implemented and the
`Option` half faked.

## Decision 1: `Map.get` and `Grid.find` answer `Option[V]`

`TypePattern` gains an `Option(Box<TypePattern>)` arm. Its own arm, not a
`CollectionCtor`: `Option` is not a collection but the one generic **enum** def
the language has (F12), and a row spelling it must lower through
`TypeDb::option_of`, which names the single canonical def every `Option[T]` in a
program shares. Registering one per row is what TY-06 was.

`Grid.find` follows by the same rule §4.7 states generally. `Grid.find_all` is
untouched: a `Vec` already encodes "nothing matched" as emptiness.

The two wrappers build their answers through the runtime's own `option_schema`
(ADR-074), whose `Some` slot is **unknown** — `V` is learned from the value found
and never from a static type — and whose nominal identity and null-tolerant slot
rule are what let the result match arms the program compiled against the
codegen's `Option[Int]`.

`MapGet`'s manifest effect moves `Pure` → `Allocates`, because building a `Some`
allocates and its call site is therefore a GC safepoint. The **catalog row's**
`Purity::Pure` does not change: the debugger's read-only gate reads `purity`, and
`purity.rs` already records that allocation is compatible with read-only.

This is a source-visible language change. `m.get(k)` used as a bare `V` stops
compiling, and the fix at every such site is `m[k]` — §4.7's other half — wherever
the key is known present, which is what every broken call site in the corpus and
the suite turned out to be.

## Decision 2: an empty `min`/`max` faults; they do **not** become `Option`

`v.min()` on an empty sequence answered `0`. That is not a *missing* answer, it
is a **wrong** one: `0` is below every element of `[3, 4]` and above every element
of `[-3, -4]`, so nothing at the call site can distinguish it from a real
minimum, and it was seeded into the accumulator rather than derived from the data
at all.

They join the three seeded sinks — `reduce`, `min_by`, `max_by` — that MIR-09
already gave `FaultKind::EmptyCollection`. `sink_finish`'s own comment had
conceded that `min`/`max` "share the empty case, but *not* the defect" and
deferred the question here.

`Option` would have been the other answer and is wrong for them: an empty `min`
is a mistake in the program, not the ordinary domain-level absence §4.7 reserves
`Option` for, and making it an `Option` would force an unwrap at every call site
for a case the caller has already ruled out.

The mechanism was already present and already wired: `sink_alloc` returns a
`seen` flag for exactly this, and `emit_empty_collection_guard` is what the other
three call. `Sink::Min | Sink::Max` splits out of the arm it shared with `Sum`,
`Product`, `Count`, `Find` and `Position`, whose empty answers really are right.

## Decision 3: `Counter.get` keeps its zero default

Unchanged, deliberately. §6.2 states that "Counter[T] behaves as a map whose
absent values read as zero", so a counter's absent value is not absence at all —
it is zero, and `praxis_counter_get` implements exactly that. Making it an
`Option` would break the one idiom counters exist for.

## Decision 4: `AbiRet` splits `Gc` from `GcUnit`, and has no third arm

Nothing in the workspace could relate a catalog row's declared result to whether
its wrapper may answer the Unit sentinel. `AbiRet::Gc` said "a `GcRef`" and
stopped, so `Map.get -> V` sitting on a wrapper that returned Unit was a
disagreement no test could reach. `allocates` and `can_fault` are derived from the
manifest precisely because a hand-written per-row copy had drifted
(`bitset_insert` declared `can_fault: false` while its wrapper raised); "can this
answer be Unit" had nowhere to live at all.

- `Gc` — a `GcRef` carrying the wrapper's **answer**, a value of the result type
  its row declares.
- `GcUnit` — **always** the Unit sentinel; the wrapper's answer is "done", not a
  value. `Vec.push`, `Map.insert`, `out`, `assert`, and eighteen more.

**There is deliberately no third arm.** "May be Unit, may be a value" *is*
RT-14/RT-15, and its absence is what makes the defect unrepresentable rather than
merely fixed: an author restoring the old behaviour must write `GcUnit` beside a
`V` result — which the sweep refuses — or `Gc`, which is a claim about the wrapper
that the runtime tests check. There is nowhere between them for it to live.

`Gc`'s doc states the two places the sentinel still appears under it, so the arm
is not quietly weaker than it reads: a **fault** return, which is the ABI's
universal "a Praxis function returns a valid `GcRef` even when it unwinds"; and,
in the handful of wrappers the codegen calls directly, a **refusal the compiler
was responsible for having prevented** (`praxis_alloc_enum` with a null schema,
`praxis_tuple_get` with an out-of-range index). Neither is "the value is absent".

### What the sweep proves, and what it does not

`a_non_faulting_row_with_a_value_result_cannot_answer_the_unit_sentinel` asserts
the biconditional over every catalog row that lowers to a runtime symbol: the row
declares `Unit` if and only if the manifest says `GcUnit`. Faulting rows are
included, because a faulting wrapper's unwind Unit is a different thing from its
declared result, so restricting the sweep would only weaken it.

The manifest row is **hand-asserted**, at exactly the trust level `Effect`
already is. So the sweep catches a catalog row disagreeing with its manifest row,
not a manifest row lying about its wrapper — reverting `map_get`'s result to
`Var("V")` while leaving `MapGet` at `-> Gc` passes it. The other half is the two
runtime tests that call the wrapper and look at what comes back
(`absent_map_get_does_not_return_an_untyped_unit_sentinel` and its `Grid.find`
sibling), which are two of the five S18 un-ignored. The test's own doc says all
of this, because a sweep that is trusted for more than it proves is worse than no
sweep.

No `const _` assertion is added in the build-time manifest sweep: the invariant
relates a manifest row to a **catalog** row, and the catalog is built at run time.
An assertion there could only have restated something already true.

## Consequences

- **Five ignored exit-criterion tests pass**, and four defect-pinning tests are
  rewritten rather than updated (plan §8.2's rule): `map_get_absent_returns_unit`
  asserted only that no fault occurred; `adv_map_get_absent_returns_unit` and the
  `.get` half of `adv_map_index_of_a_missing_key_faults_where_get_answers` checked
  absence by comparing the answer against `Int 0` and expecting *false*; and
  `infer_tests`'s `!has_type_error("… -> Int { m.get(\"a\") }")` inverts.
  `adv_pipeline_empty_source_min_is_zero` is the fifth, rewritten for Decision 2.
- **One corpus program changes.** `day10_bfs_shortest_distance.px` reads a
  distance for a node it enqueued three lines earlier; that is the assertion-like
  spelling and is now `dist[node]`.
- **`prelude.rs` stops claiming `Option` is "returned by find/position on a
  miss".** Those answer an `Int` index with a `-1` sentinel. Whether *that* should
  change is a separate question and is not S18's.
- **No new diagnostic code.** The language change surfaces as ordinary `Y001`
  type mismatches at the call sites it breaks, which is the right report; ADR-051
  is unchanged, and `Y022`/`Y116`/`Y126`/`N008` remain free.
