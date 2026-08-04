# ADR-126: A pipeline materializes on its own, and `collect` named a step it takes anyway

**Date:** 2026-08-04
**Status:** Accepted — implemented

## Context

```praxis
var a = values.map(|x| x * 2)
var b = values.map(|x| x * 2).collect()
```

The two lines built the **same** `PipelinePlan` — `links: [Map]`, `sink:
Collect` — and therefore the same fused loop and the same `Vec[Int]`. The
second one is not an optimization of the first and it is not a different
program; it is the first one with a word in it.

The word is a leftover from a design that did not ship. §6.3 was specified
around a lazy `Seq[T]`, and in a lazy pipeline `collect` is load-bearing:
something has to say where the laziness ends. ADR-028 decision 2 then shipped
the pipeline **eagerly** — every catalog row's result pattern is
`vec_of_t`/`vec_of_u`, no row produces a `Seq` — and it recorded the reason in
the user's own words: *"no `.collect()`"*, *"it should just work"*. ADR-029 kept
that surface while adding fusion, and `recognize_pipeline` is where it kept it:

```rust
None => {
    let link = classify_link(db, name, args, *result_ty)?;
    (Some(link), Sink::Collect)   // the chain ends on a stage → append the sink
}
```

So the method survived the decision that removed its job. It stayed
`Stability::Stable` in the catalog for three milestones, and the evidence that
nobody wanted it is that nobody wrote it: **zero** calls across
`tests/aoc-corpus/`, the CLI fixtures and `benchmarks/`. Every use in the tree
was in a Rust-embedded test source, and every one of those was redundant.

## Decision 1: the catalog rows go, and the sink stays

`seq_collect_on_vec` and `seq_collect_on_seq` are deleted, and the `("collect",
[])` arm of `classify_sink` with them. `Sink::Collect` is untouched — it is the
sink `recognize_pipeline` *appends*, and after this change appending is the only
way it is ever selected. Nothing about how a chain materializes changes; what
changes is that the materialization has one spelling instead of two, and the
one it has is no spelling at all.

The removal is a `Y110` for a program that writes `v.collect()`, which is the
same report that type has always given for `v.chunks()` and `set.collect()` —
so the diagnostic surface gains no new shape.

**`seq_collect_on_seq` was already unreachable.** No catalog row answers a
`Seq[T]`, which the block comment above the barriers has said since M8-WS11, so
all twenty `Seq`-receiver rows are dead and this deletes two of them. The other
eighteen stay: they are a real half of a pair whose `Vec` half is live, and they
become reachable the day a row answers a `Seq`. A `collect` row would too — and
that is the condition under which this decision should be revisited, not a
reason to keep it now.

## Decision 2: the shallow copy it also did is not preserved

On a bare `Vec` with no stages in front, `v.collect()` was a `Collect` sink over
zero links: a loop that pushed each element into a fresh `Vec`. That is a
shallow copy, and it was the **only** one in the catalog — there is no `clone`,
no `copy`, no `to_vec`.

It goes anyway, undeprecated, because it was never designed:

- Nothing in the catalog, the design document or the row's own doc string
  ("Materialize the elements into a Vec.") says `collect` copies. A caller who
  relied on it relied on an inference from the implementation.
- It reads as the opposite of what it does. Everywhere else in the language a
  collection is a reference and passing one shares it (§10.4); a method whose
  name is about *sequences* is not where a reader looks for the exception.
- No program in the tree used it. The one test that did — the JIT's
  `pipeline_collect_materializes` — was testing the sink, not the copy, and now
  tests the sink through a stage.

If `Vec` should be copyable, it should be copyable by a method that says so, on
every collection that has the same need, and it should be its own decision. This
one only declines to leave the capability behind under a name that does not
mention it.

## Consequences

- `praxis_technical_design.md` §6.3 loses the `collect` bullet and gains the
  reason. §5.7's example rows, which still wrote `-> Seq[U]` and `-> Seq[Point]`
  against a catalog that answers `Vec`, are corrected in the same pass — the
  same stale lazy-pipeline residue, in the one section a reader checks a
  signature against.
- The catalog's intrinsic count drops by two;
  `intrinsics_are_all_recognized_so_there_is_no_second_lowering` walks what is
  left and its `checked >= 40` floor still holds at 47.
- Eleven Praxis-source snippets in the Rust tests drop a `.collect()` and assert
  the same values, which is the claim this decision rests on stated as a
  regression: if the implicit sink were not equivalent, they would not.
