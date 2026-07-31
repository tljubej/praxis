# ADR-059: A range is a value, `..` is half-open, and a descending range is empty

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S17 — TY-34)

## Context

`CollectionCtor::Range` existed in `praxis-stdlib`'s type patterns and
`capability::iter_item` already answered that a range yields `Int` — but there
was no syntax to build one, no runtime object behind it, and
`praxis-repr::descriptor_for_type` refused it outright ("Range has no runtime
object (design decision D6)"). That middle state is TY-34, and D6 answers it:
**implement the full vertical slice**, against the eight-site deletion.

D6 leaves four sub-decisions owed, each with a recommendation. All four are
answered here as recommended; the reasons are below because a recommendation is
not a rationale.

One thing was already done: **the lexer.** `DOT2` and `DOT2EQ` were tokens, the
number scanner already refused to read `1.` as a float when a second `.` follows,
and three lex tests pinned it (`range_not_lexed_as_float`,
`inclusive_range_not_lexed_as_float`, `float_then_range_boundary`). The slice is
five layers, not six.

## Decision 1: `..` is half-open, `..=` is inclusive, and both bounds are required

`a..b` is the integers from `a` up to but **not** including `b`. `a..=b` includes
`b`. This is what `iter_item`'s `Int` element already implied and what every
neighbouring language does; the alternative — an inclusive `..` — makes
`0..v.len()` an off-by-one in the most common range a program writes.

There is no `a..`, `..b` or `..`. A range is a collection with a length, and a
`for` loop over one reads that length; an open end has no length to read. Making
the bound required means the AST has no "missing bound" state a lowering has to
invent a value for.

## Decision 2: a range is a first-class value

`CollectionCtor::Range` already made it a *type*, so making it only a `for`-header
form would have left the type unconstructible — the same middle state TY-34 is.
So `0..5` binds to a `let`, passes as an argument, comes back as a result, lives
in a `Vec`, and is a `Map` key.

Being a key is a decision, not a consequence. ADR-057 D4 rejected mutable
collections as keys, and `supports_hash_stable`'s collection arm was a blanket
`false` — every collection, because every collection that existed then was
mutable. **A `Range` has no mutator at all**, so its two bounds are as fixed as a
tuple's elements, and a tuple of scalars has always been a key. The arm is now
per-ctor, so the rule reads as what D4 said it was: mutability, not
container-ness.

**A bare nullary collection name in type position is the type it names.** This is
the annotation half of "first-class", and it was broken: `named_type` handled a
bare `Ident` and never consulted `collection_from_name`, which is only reached
for a *bracketed* name. So `fn f(r: Range)` and `fn f(b: BitSet)` — the only two
ctors with no type arguments, and therefore the only ones written bare —
resolved to **nothing**, and the parameter silently became a fresh variable that
unified with whatever the body did to it. `fn total(r: Range) { for i in r { … } }`
reported "values of type `Int` cannot be iterated". `BitSet` had the same hole and
inherits the fix. A bracket-less `Vec` now reports `Y007` ("expected 1 type
argument, got 0") instead of a silent variable, which is exactly what that code
is for.

## Decision 3: a descending range is empty, and the constructor is where that is decided

`5..0` has no elements. It does **not** count down: a range that silently
reversed direction would compute a different loop than the one written, half the
time it appeared, and `0..n` with `n == 0` is the case that decides it — an empty
collection's loop must not run backwards over the whole range.

`RangeVal::new` normalizes an `end` below `start` *to* `start`, so **no range with
a negative length exists**. That is not a convenience: `len` is what a `for` loop
reads, and a negative length is a loop bound nothing checks. The invariant is
established by the only constructor and there is no mutator, so it holds by
construction rather than by every reader remembering to clamp.

For the same reason `..=` is normalized at construction rather than carried as a
flag: an inclusiveness bit would be a second spelling for one set of integers, and
every consumer would have to remember which it had. `1..=4` and `1..5` are the
same payload, hash to the same key, and render the same.

## Decision 4: the bounds are `Int` only

`iter_item` says a range yields `Int`. A `Float` range would need a step to yield
anything at all, and `0.0..1.0` has no elements without one — so admitting float
bounds would make `iter_item` a lie with nothing to fix it. D6 recommended `Int`
only "until TY-31's numeric constraint lands"; TY-31 landed and delivered
`Bound::Is(ScalarType)` rather than a capability (ADR-057 D6), and ADR-058 made
the same call for the numeric prelude helpers for the same reason. `Int` it is.

## Decision 5: `..`/`..=` binds looser than comparison, and a newline after it continues

The precedence had to be **inserted**, not appended: range is `bp(3, 4)`, between
`||` and comparison, and comparison/additive/multiplicative/prefix all shifted up
by two. That is what makes `0..n - 1` mean `0..(n - 1)` — every range in the
corpus writes an arithmetic bound — and what makes `a..b == c..d` compare two
ranges rather than range over a `Bool`.

It builds a `RANGE_EXPR`, not a `BIN_EXPR`. A range is not an operator applied to
two numbers; it is a collection built from two bounds, and every consumer that
asks `BinExpr::op()` would otherwise have to answer "none of the operators I
know". `RangeExpr::is_inclusive` reads the operator token, so the syntax is the
single source of that fact.

A newline after `..` **continues** the expression, exactly as one after `+` does.
D6 flagged this as needing its own decision because of FE-04; it does not need a
special case, because D8's rule is already "never inside the Pratt loop" and the
loop does not consult `newline_before`. `a_range_continues_across_a_line_break` is
the test that says a range did not become the exception.

## Decision 6: `len` faults rather than reporting a wrapped count; `get` faults out of range

`praxis_range_len` computes `end - start` in `i128` and range-checks it. Only the
very widest ranges fail (`Int::MIN..Int::MAX` holds `2^64 - 1` integers), and the
alternative is worse than a fault: an `i64` subtraction wraps *negative* there, so
a `for` loop over every integer would have run zero times. The kind is
`InvalidSize` — a range the runtime cannot honour — for the reason ADR-058 gives
about clamp: S17's one ABI bump is spent (H17). A dedicated empty-range kind is
owed to whichever stage next spends one, and these two are its cases.

> **Superseded in part by ADR-075.** S18 spent the bump and added
> `FaultKind::EmptyRange`, but did **not** put `praxis_range_len` in it: the
> range this fires on is `Int::MIN..Int::MAX`, the fullest one expressible, so
> "empty range" would be a fault message that contradicts its own input. It
> raises `IntOverflow`, which is what `gcd`, `lcm` and A\*'s path cost already
> answer for a result with no `Int`. `clamp` alone is `EmptyRange`.

`..=Int::MAX` is the one inclusive range whose exclusive end does not exist. It
**saturates** rather than faulting, and the saturation costs one index at the very
top: the range is perfectly well defined, and there is no `Int` above `Int::MAX`
that it would otherwise have excluded.

## Consequences

- **`RUNTIME_ABI_VERSION` stays 13.** `BuiltinTypeId::Range` is appended, so
  every existing id is unchanged and no `#[repr(C)]` layout moved. `COUNT` and the
  `BUILTINS` array are compile-time facts inside one binary, not an offset
  generated code reads. Same reasoning as ADR-058's seven new symbols.
- **`Seq` is now the only `CollectionCtor` with no runtime object.** Two tests
  used `Range` as their witness for "a type that cannot exist" and were
  **rewritten** to use `Seq`:
  `a_known_element_type_with_no_descriptor_fails_the_compile` (codegen `lower.rs`)
  and `a_type_with_no_runtime_object_has_no_descriptor` (`praxis-repr`). The
  property is unchanged; only the witness moved. Each gained a positive
  counterpart asserting that `Range` *does* have a descriptor now.
- **§3.3's representative program runs.** `sign`, `abs`, `max` (ADR-058) and
  `0..=distance` all work; what is left in that program is
  `Counter[(Int, Int)]()` — explicit type arguments on a constructor call, which
  the grammar does not have and which no finding names.
- **A `for` over an *unannotated* parameter still unifies it with its own element
  type.** `iter_item` answers an unresolved iterator with *itself* (the optimism
  ADR-057 records), so `fn total(r) { for i in r { t = t + i } }` pins `r` to `Int`
  and reports `Y005`. This affects `Vec`, `BitSet` and `Range` identically and is
  not TY-34's; it is why the range gates annotate.
- **A range renders as its normalized half-open form.** `out(1..=3)` prints
  `1..4`. That is the point of normalizing, and it is the one place the spelling
  the program wrote is not recoverable.
