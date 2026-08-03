# ADR-115: A `Text` counts itself once, and the count is the licence to index its bytes

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance round, package W2 (handover 25 §5 F-8,
handover 26 §4)
**Amends:** ADR-013, which fixed the two `Text` representations, and ADR-022,
which chose the source slice. Neither changes shape; what changes is that the
owned payload now carries a number about its own bytes, and that number is
observable in the complexity of `t.len()` and `t[i]` rather than in their
answers.
**Depends on:** ADR-111, whose "UTF-8 is the caller's precondition, checked at
the one door raw bytes enter" is what lets Decision 4 delete a validation
instead of moving it.

## Context

`t.len()` was `chars().count()` and `t[i]` was `chars().nth(i)`, and
`iter_plan` gives `Text` an `InPlace { TextLen, TextGet }` plan — so `for c in t`
walked the string from the start for every character. This is a
correctness-of-complexity bug on the shape the language exists for: the user's
own `test.px` in this tree is a subscript into a text, and every input-parsing
program indexes the captures it just parsed.

**It was quadratic twice, and the second one is not in handover 25.** `lower_for`
puts the plan's `len` call in the loop **header**, not in the preheader, so
`for c in t` re-evaluated `praxis_text_len` once per iteration. Handover 25 §5
F-8 names only `t[i]`. Both halves are O(n) per step over the same string, so
`for c in t` cost two passes per character and 2n² byte reads over the loop.

**It was worse than either, because both wrappers went through `text_str`.**
`text_str` calls `std::str::from_utf8` so it can hand back a `&str`, which is a
third pass over every byte on every call — and `praxis_text_is_empty`, an O(1)
question, paid it too.

**And there was a fourth pass nobody had named.** `SourceSlice::new` validated
the *whole owner* with `from_utf8` for the sole purpose of calling
`str::is_char_boundary` twice. Every capture the input parser allocates goes
through it, so parsing an n-byte input into k captures was O(n·k) before the
program indexed anything at all. That is Decision 4.

### The constraint the decision had to fit inside

`TextPayload` is a `#[repr(C)]` enum: a 4-byte tag, four bytes of padding, and a
union of `Box<str>` (16) and `SourceSlice` (24). So it is **32 bytes**, its
block is 48 with ADR-039's 16-byte header, and the owned variant leaves 8 bytes
of that union unused.

Handover 26 §9 recorded the 32 as measured on a standalone copy of these
declarations rather than on this tree. **It is measured here now**, by a
`const _` in `text.rs` that also pins `SourceSlice` at 24 and `OwnedText` at 24,
because the whole decision is affordable only while that number does not move.
What it would cost if it did move is arithmetic, not a guess — measured off
`page.rs` at this tree, where `PageHeader` is 584 bytes and the first block sits
at offset 592 in a 32768-byte page:

| payload | block | blocks per page | per-object bytes |
|---:|---:|---:|---:|
| **32** | **48** | **670** | — |
| 40 | 56 | 574 (−14.3%) | +16.7% |
| 48 | 64 | 502 (−25.1%) | +33.3% |

The pacer charges `stride + owned_bytes` (ADR-112, `text_owned_bytes`), so a
size-class move is a straight +16.7% pacing charge on **every** `Text` object in
every program — including the parser's captures, which are pure stride because a
slice owns no bytes. That is the bar every alternative below had to clear.

## Decision

### 1. The count is one lazily filled `Cell<u64>` on the owned payload, and it is also the ASCII test

`TextPayload::Owned` now carries an `OwnedText { bytes: Box<str>, char_count:
Cell<u64> }` — 24 bytes, into the 8 the union already reserved.
**`size_of::<TextPayload>()` is still 32, the block is still 48, the size class
does not move, and nothing in `praxis-mir` or `praxis-codegen-cranelift`
changes.** Generated code never reads this layout: there is no `offset_of!` on
`TextPayload` anywhere in the tree, and no manifest row's effect changed, so
this owes no `RUNTIME_ABI_VERSION` paragraph and does not contend for the
round's single bump.

`u64::MAX` is `NOT_COUNTED`. It is a sentinel in the strong sense rather than a
convenient one: a text with `u64::MAX` scalars needs `u64::MAX` bytes, and the
`Box<str>` holding them cannot exist on a 64-bit host, so "counted, and the
count happens to equal the sentinel" is unreachable rather than unlikely.

**One field, not two.** `char_count == bytes.len()` iff every scalar in the text
is one byte, so the count *is* the ASCII test and there is no separate flag. A
flag would be a second thing that has to stay true about the same bytes, and the
pair `(count, flag)` can express a text that claims three scalars in five bytes
and claims to be byte-indexable. The equivalence needs the bytes to be *valid*
UTF-8 — a leading byte at or above `0x80` is always followed by a continuation
byte — and that is the payload's standing invariant, stated by `text_str`'s
`expect` and enforced by ADR-111 at the one door.

**Lazy, and deliberately against handover 25 §5 F-8's phrasing** ("record
`is_ascii` on the text payload at construction"). `praxis_get_input`'s buffer is
one owned payload and can be tens of megabytes; a program that reads its input
and never indexes a text would pay a full scan of it for nothing. Every caller
of the count is already asking a question whose honest answer is a scan.

The fields are private and `OwnedText::new` is the only constructor, so a
payload whose count does not describe its bytes is unconstructible. That matters
more than it looks: a stale count is not a slow program, it is a wrong one —
`t.len()` would answer somebody else's length and `t[i]` would index bytes in a
text that has multi-byte scalars. `Text` is immutable (ADR-085 allocates a fresh
owned payload for `+`), so there is no path that could invalidate one, and
`a_concatenation_counts_its_own_bytes_and_not_an_operands` is the test that says
so out loud.

### 2. The count lives on the **owner**, and a slice inherits the licence rather than caching its own

This is the question handover 26 §4 left open, and it is the one that decides
whether the package is worth anything. §4 says the 8 spare bytes are the ones
"`Owned` never uses" — true — and stops there. But **the texts a program indexes
are mostly not owned.** `praxis_get_input` allocates one owned buffer; every
capture the input parser hands back is a `Slice` of it (`parser.rs`'s three
`alloc_text_slice` sites), and `Input::new` collapses owner chains so each is
exactly one level deep. A count that only existed on `Owned` would leave `for c
in line` exactly as quadratic as it was.

It does not, because **the licence is inheritable and the count is not needed to
transfer it.** A view of a text whose every scalar is one byte has one scalar per
byte, so:

- a slice's character count is its **byte length**, in O(1); and
- `slice[i]` is `bytes[i]`, in O(1);

both read off the *owner's* cached count, which is computed once and answers for
every view of that buffer forever. `SourceSlice::new` already refuses ends that
split a scalar, so no further check is owed.

When the owner has a multi-byte scalar anywhere, a slice has no cached count of
its own and counts its own bytes — O(its own length). **That residual is real and
it is named here rather than glossed.** The reason it is acceptable is not that
non-ASCII input is rare, though it is: it is that on such a text `t[i]` is O(i)
*whatever* is cached, because there is no random access into a variable-width
encoding without a wider representation. So caching a slice's own count would
convert no quadratic loop into a linear one. It would buy exactly one thing — an
O(1) `t.len()` on a slice of a multi-byte owner — and that is the thing the three
alternatives below are priced against.

**The three ways to give every payload its own count, and what each costs.**

- **Shrink `SourceSlice`'s `start`/`len` to `u32`** (16 bytes), and wrap the enum
  in a struct with a `Cell<u64>` in front: 8 + 24 = 32, zero bytes, count on both
  variants. **Rejected**, because it bounds a *view* at 4 GiB and the failure
  mode is a lie. `SourceSlice::new` would refuse the range, `alloc_text_slice`
  would answer `None`, and the parser turns `None` into a `ParseFail` — whose own
  doc says `None` "means the interpreter has a bug". A 5 GiB input would report a
  spurious parse failure, and no test in this tree can be written that would ever
  see it. Buying an O(1) length on a rare shape by introducing an untestable
  cliff on a rarer one is not a trade, it is a swap of a slow case for a wrong
  one.
- **Hand-roll the discriminated union** — `#[repr(C)] struct { tag: u32,
  char_count: Cell<u32>, body: union { … } }` — putting the count in the four
  bytes the `#[repr(C)]` enum already wastes between its tag and its union. Also
  32 bytes, also a count on both variants, and no bound on offsets. **Rejected**
  on the maxim this repo is written to: a `union` with a `ManuallyDrop<Box<str>>`
  and a hand-written `drop` replaces a compiler-checked enum with a shape whose
  illegal states are exactly the ones the enum makes unrepresentable, and it does
  it in the one payload the collector's `drop_value` walks. A `u32` count would
  also need its own degradation story past 4 G scalars. The cost is paid in the
  one place — a sweep-time `drop_in_place` — where getting it wrong is a
  use-after-free rather than a wrong answer.
- **Grow the payload to 40 bytes** and put a `Cell<u64>` on `SourceSlice`. Honest,
  safe, no cleverness — and it costs the size class: block 48 → 56, 670 → 574
  blocks per page, **+16.7% pacing charge on every `Text` object in every
  program**, including the many-slice programs this package exists to serve. A
  program that parses ten thousand fields and indexes none of them pays that for
  nothing. **Rejected on the arithmetic.**

Choosing the owner is what makes the package cost zero bytes, and the reason it
is not merely the cheap answer is the one in the paragraph above: the thing the
other three buy cannot turn a quadratic loop linear.

### 3. What this fixes, and what it does not

Stated as the four cases, because "O(1) indexing" without the qualifier would be
false:

| the text | `t.len()` | `t[i]` | `for c in t` |
|---|---|---|---|
| owned, one byte per scalar | O(1) after the first | O(1) | **O(n)** |
| owned, has a multi-byte scalar | O(1) after the first | O(i) | O(n²) |
| slice of a one-byte owner | O(1) | O(1) | **O(n)** |
| slice of a multi-byte owner | O(its own length) | O(i) | O(n²) |

**Both of the quadratics handover 26 named are fixed on the first and third
rows**, which is every ASCII text and therefore essentially all input. The loop
header's re-read is fixed by the count alone and so is also fixed on row two.
Row four is the residual Decision 2 prices, and on it neither is fixed.

The header still emits a runtime call and an `Int` box per iteration; that is a
constant factor, not a complexity, and removing it is the ADR-108 hoist named in
the open questions. It is not this package's to take: `Text` is immutable, so
`praxis_text_len` over a loop-invariant iterable is loop-invariant, but the hoist
is a `praxis-mir` builder change and this round has other packages in that file.

`praxis_text_is_empty` stops going through `text_str` and asks the bytes: a text
is empty iff it has no bytes, since no scalar encodes to zero of them. That is an
O(n) answer to an O(1) question deleted, and it was `Effect::Pure` the whole
time.

### 4. `SourceSlice::new` stops re-validating its owner, because ADR-111 already settled the question

Two byte comparisons replace `std::str::from_utf8` over the owner's whole byte
range. `is_scalar_boundary` is `str::is_char_boundary`'s test spelled on bytes —
a position begins a scalar iff its byte is not a continuation byte, and the end
position always does — and it is the entire content of what the two
`is_char_boundary` calls did.

The deleted validation answered a question that was already answered. Since
ADR-111, `praxis_alloc_text`'s callers owe UTF-8 as a precondition, the one
caller holding raw host bytes (`praxis_get_input`) validates them and raises
`InvalidText` there, and `text_str` states the invariant by `expect`ing it. A
`Text` whose owner is not UTF-8 is already a defect that surfaces as a panic in
`text_str`; re-deriving the same fact per slice allocation is what made parsing
O(n·k).

`taking_a_view_does_not_walk_the_owner` allocates eight thousand views of a
256 KiB owner. Under the old constructor that is two billion byte validations and
the test does not finish; there is no assertion to make about that beyond the
test returning, which is the idiom
`reading_a_deep_slice_chain_does_not_recurse` in the same file already uses.

**What this gives up, stated plainly:** a `Text` built by a host that violated
`praxis_alloc_text`'s precondition used to be refused a slice and will now be
sliced. It was already an aborting defect one call later. The check that a view's
ends do not split a scalar — the one RT-06 was about — is unchanged and still
tested by `an_out_of_range_or_non_boundary_slice_is_unconstructible`.

### 5. The non-ASCII cursor is declined, and here is the arithmetic

Handover 25 §5 F-8 proposes a one-entry `(char_index, byte_offset)` cursor
cached on the payload, making sequential access into multi-byte text O(1)
amortized. Handover 26 recommends declining it and recording the refusal with
its numbers. **Declined**, on three counts, and the first is the one that
settles it:

1. **It cannot reach the case it is for.** The cursor would live where the count
   lives — on the owned payload — and a cursor on the *owner* is useless to a
   slice, because a slice's sequential walk is over its own bytes at its own
   offsets. The multi-byte texts a program actually iterates are the parser's
   captures. A cursor that helps only owned multi-byte text helps `for c in
   "héllo"`, a literal, and nothing a program reads.
2. **It costs a size class, and the cursor is what pushes it over.** Packed into
   a pair of `u32`s it is 8 bytes: `OwnedText` goes 24 → 32, `TextPayload` 32 →
   40, block 48 → 56, 670 → 574 blocks per page, **+16.7% on every `Text` in
   every program** — paid by the ASCII programs that are 100% of the benchmark
   suite to speed up a shape none of them has. As two `u64`s it is 16 bytes and
   two rungs: block 64, 502 blocks per page, +33.3%.
3. **It is a cache with an invalidation story**, where the count has none. The
   count is a function of bytes that never change. A cursor is a function of the
   last access, so it is per-payload mutable state that two interleaved walks of
   the same text thrash, and its correctness argument is about *sequences* of
   calls rather than about the bytes.

The row it would fix is row two of Decision 3's table, and only for owned text.
If it is ever revived, the honest form is not a cursor on this payload but a
different `Text` representation for multi-byte text — which is a §4.3 question,
not a caching one.

## Measurement

Apple M2 Pro, 16 GiB, macOS 26.6, release builds, `/tmp/praxis-arms/W2-a` and
`/tmp/praxis-arms/W2-b` exactly as the build phase staged them and neither
rebuilt. **The headline is not a speedup, it is the shape of a curve**, because
that is the claim this package makes and it is the one claim a noisy machine
cannot spoil: a ratio at one size is a speedup, a ratio that doubles when the
input doubles is a complexity fix.

### What was run, and how

`benchmarks/ab.py` **cannot measure this and was not made to.** Its `--only`
argument is validated against `run.BENCHMARKS` and `die`s on any other name, and
its runner feeds `f"{size}\n"` on stdin — one integer, never a file. Both are
correct for the eight-benchmark suite and both make it structurally unable to
run a program that reads text. So its *discipline* was borrowed rather than its
code: the same exclusive `/tmp/praxis-measure.lock`; an untimed warm-up run per
(binary, program, input) before the clock starts, for the XProtect exec scan the
load gate cannot see; **A,B,B,A per rep with the leading arm alternating, five
reps** — twenty timed runs per cell, ten paired A/B adjacencies; **every run's
stdout compared byte-for-byte against the other arm's**, with a hard exit on the
first difference; median per arm, and the median of the paired ratios as the
headline. No run in any table below differed by a byte between the arms.

**The load gate was recorded rather than enforced**; see the caveat at the end
of this section for why, and for what it can and cannot have done to these
numbers. The four programs are:

```praxis
// walk — `for c in t` over a Text read from stdin; the shape `test.px` uses.
let t = read rest
var total = 0
for c in t { total = (total + c.to_int()) % 1000003 }
out(total)

// index — n random subscripts into an n-scalar Text, the length hoisted by hand
// so the loop header's re-read is not what is being timed.
let t = read rest
let n = t.len()
var total = 0
var i = 0
var k = 0
while k < n {
    i = (i * 7919 + 13) % n
    total = (total + t[i].to_int()) % 1000003
    k = k + 1
}

// puzzle — parse into captures, then walk each capture. Eight long lines, so
// the per-capture cost grows with the input rather than the capture count.
let ws = read lines(word)
for w in ws { for c in w { total = (total + c.to_int()) % 1000003 } }

// parse — many captures, none of them indexed: quadratic #3 alone.
let rows = read lines(`{a:word} {b:word}`)
out(rows.len())
```

### The asymptotics

Median seconds per arm; `A×` and `B×` are each arm's growth over the previous
row, and `A/B` is the median of that cell's ten paired ratios.

**`for c in t`, a slice of the input buffer, ASCII** — Decision 3's row three:

| n (bytes) | arm A | arm B | A× | A/B |
|---:|---:|---:|---:|---:|
| 16,384 | 0.0292 | 0.0051 | — | **5.5×** |
| 32,768 | 0.0982 | 0.0064 | 3.37 | **15.0×** |
| 65,536 | 0.3721 | 0.0083 | 3.79 | **44.5×** |
| 131,072 | 1.4663 | 0.0117 | 3.94 | **125.4×** |
| 262,144 | 5.8378 | 0.0189 | 3.98 | **309.1×** |

**`t[i]`, n subscripts into an n-scalar slice, ASCII** — row three again, with
the loop header taken out of it:

| n | arm A | arm B | A× | A/B |
|---:|---:|---:|---:|---:|
| 16,384 | 0.0193 | 0.0055 | — | **3.5×** |
| 32,768 | 0.0612 | 0.0068 | 3.17 | **8.9×** |
| 65,536 | 0.2214 | 0.0098 | 3.62 | **22.5×** |
| 131,072 | 0.8514 | 0.0155 | 3.85 | **55.2×** |
| 262,144 | 3.3365 | 0.0264 | 3.92 | **126.3×** |

**`for c in w` over the parser's captures** — the shape a puzzle program has,
and the one Decision 2 exists for. Eight lines, each n/8 bytes:

| n | arm A | arm B | A× | A/B |
|---:|---:|---:|---:|---:|
| 65,536 | 0.0564 | 0.0077 | — | **7.3×** |
| 131,072 | 0.1929 | 0.0113 | 3.42 | **17.2×** |
| 262,144 | 0.7356 | 0.0193 | 3.81 | **38.2×** |
| 524,288 | 2.8586 | 0.0339 | 3.89 | **84.2×** |

Arm A quadruples per doubling on all three. Least-squares slope in log-log, on
each arm's time net of a same-size control program (`let t = read rest;
out(t.len())`, which is process start plus the read plus one scan and costs 3.1
to 5.5 ms across these sizes):

| | arm A exponent | arm B exponent |
|---|---:|---:|
| `for c in t` | **1.955** | 0.98 |
| `t[i]` | **1.924** | 0.98 |
| `for c in w` | **1.919** | ~0.97 |

**Arm B's exponent is quoted from a larger sweep, and the reason is stated
rather than hidden.** At the sizes above, arm B's whole runtime is 5 to 34 ms
against a 3 to 5 ms floor, so subtracting the control removes most of what is
left and the fitted slope reads 0.69 to 0.90 — a floor artifact, not a
sub-linear algorithm. Run alone at sizes where the floor is noise, arm B is flat
linear:

| n | `for c in t` net | ×prev | `t[i]` net | ×prev |
|---:|---:|---:|---:|---:|
| 1,048,576 | 0.0529 | — | 0.0824 | — |
| 2,097,152 | 0.1064 | 2.01 | 0.1580 | 1.92 |
| 4,194,304 | 0.2055 | 1.93 | 0.3153 | 2.00 |
| 8,388,608 | 0.4065 | 1.98 | 0.6358 | 2.02 |

— slopes **0.977** and **0.984**; the capture program over the same range is
1.99 and 1.91 per doubling. Arm B walks 8.4 MB of text in 0.41 s where arm A
needed 5.8 s for 262 kB.

**The owned row, measured separately and under a weaker protocol.** `test.px`'s
own shape is a literal, which is an `Owned` payload rather than a slice, so it
is Decision 3's row one. Programs with an n-byte literal in the source and no
stdin, median of seven, arms run back to back rather than palindromically —
enough to show the curve, not enough to quote a ratio to three figures:

| n | arm A | arm B | A× | A/B |
|---:|---:|---:|---:|---:|
| 16,384 | 0.0278 | 0.0044 | — | 6.3× |
| 32,768 | 0.0960 | 0.0053 | 3.46 | 18.2× |
| 65,536 | 0.3680 | 0.0070 | 3.83 | 52.7× |
| 131,072 | 1.4359 | 0.0102 | 3.90 | 140.8× |

### Row four is not fixed, and it measures exactly as unfixed as claimed

Decision 2 prices a residual out loud, and it is worth as much as the wins above
that it was measured rather than asserted. The same `for c in t` program, same
sizes, over an input that is ASCII except for **one two-byte scalar at the very
end**:

| n | arm A | arm B | A× | B× | A/B |
|---:|---:|---:|---:|---:|---:|
| 16,384 | 0.0657 | 0.0658 | — | — | 1.00 |
| 32,768 | 0.2520 | 0.2510 | 3.83 | 3.96 | 1.00 |
| 65,536 | 0.9788 | 0.9807 | 3.88 | 3.94 | 1.00 |
| 131,072 | 3.9110 | 3.9240 | 4.00 | 4.01 | 0.99 |

Exponent **1.986** in arm A and **1.988** in arm B. One `é` in a 128 kB input
costs 3.9 s where the same program over the same bytes without it costs 12 ms —
a factor of **330** paid for a single scalar, because the licence lives on the
owner and one multi-byte scalar anywhere revokes it for every view. That is
Decision 2's trade, and the number it costs when it loses is now on the record
next to the number it wins.

### Quadratic #3 is in **both** arms, so it has no A/B number and gets a curve instead

Decision 4 is deliberately in the baseline (see the first consequence below), so
the parse-only program is arm-invariant by construction and measures so:

| lines | captures | bytes | arm A | arm B | A× | A/B |
|---:|---:|---:|---:|---:|---:|---:|
| 50,000 | 100,000 | 700 kB | 0.0218 | 0.0220 | — | 0.99 |
| 100,000 | 200,000 | 1.4 MB | 0.0413 | 0.0416 | 1.89 | 0.99 |
| 200,000 | 400,000 | 2.8 MB | 0.0826 | 0.0829 | 2.00 | 1.00 |
| 400,000 | 800,000 | 5.6 MB | 0.1648 | 0.1667 | 2.00 | 0.99 |

**The curve is the evidence, and it does not need an arm.** Both n and k double
per row, so the old `SourceSlice::new` predicts a slope of 2 and a 4× step; the
measured slope is **1.035** and the step is 2.00. 800,000 `word` captures out of
5.6 MB in 165 ms is 206 ns per capture; under O(n·k) that row alone is 4.5×10¹²
byte validations. The unit test `taking_a_view_does_not_walk_the_owner` is the
other half of the same statement from the opposite side — it is written at sizes
the old constructor cannot finish.

### Does every program that reads input get faster? For the suite: no, and not by a little

**No, and the reason is sharper than "a negligible fraction".** Quadratic #3
charged one whole-owner validation *per allocated source slice*, and in
`parser.rs` only `word`, `identifier` and `text`/`rest` allocate one — `int`,
`uint`, `float`, `byte`, `digit` and `char` build their scalar from the bytes and
allocate no `Text` at all. **All eight benchmarks are `let n = read int`**, over
a stdin of at most eight bytes (`hashwork`'s `9400000\n`; `bfs`'s is `200\n`).
So `k = 0`: the suite made *zero* calls into the constructor Decision 4 fixed,
and the win there is not small, it is absent. `bfs` and `vm` "read structured
input" only in the sense that they *build* it — the grid and the bytecode are
constructed in the program from one integer, not parsed.

The measurable floor agrees: `let n = read int; out(n)` on that stdin is 2.98 ms
in both arms, and that is process start and code generation, not parsing.

### The suite, and why 0.994× is the right answer

`ab-W2.json`, all eight benchmarks, five reps, both arms:

| | speedup | resolution bar | resolved? |
|---|---:|---:|---|
| `primes` @ 1,600,000 | 1.000 | 2.0% | no |
| `mandelbrot` @ 430 | 1.006 | 2.0% | no |
| `collatz` @ 340,000 | 1.003 | 2.0% | no |
| `vm` @ 2,800,000 | 0.996 | 2.0% | no |
| `hashwork` @ 9,400,000 | 1.001 | 2.0% | no |
| `tree` @ 330 | 0.998 | 2.0% | no |
| `pipeline` @ 1,000,000 | 1.005 | 2.0% | no |
| `bfs` @ 200 | 0.947 | 9.4% | no |

Geometric mean **0.994×**, **8 of 8 unresolvable**. This is the predicted result
and it is not a disappointing one: it is the suite reporting, correctly, that it
does not contain the shape. No `.px` in `benchmarks/` iterates a `Text`, seven of
the eight contain no double quote at all, and every one of them reads a single
integer. A suite number that had moved would have needed explaining, because
there is no mechanism by which this package could have moved it — and `bfs`'s
0.947 is the one cell whose own paired dispersion (9.4%) is larger than its
delta, which is what a wide bar is for.

The consequence for how this repo measures is the general one: **a suite that
does not contain a shape cannot price a change to it**, and running the suite
anyway was still worth doing, as the check that nothing regressed.

### The load caveat

The 1-minute load average was **2.44 at the start of the A/B sweep and 2.47 at
the end** (2.21 → 2.27 for the `ab-W2.json` suite run, taken with `ab.py
--max-load 6`). §6's quiescence definition asks for under 0.5 and this machine
cannot reach it: the editor's own UI holds it at 2–3 indefinitely with nothing
building, which is the case `ab.py`'s `--max-load` waiver was added for. No
`cargo build`, `cargo test` or `just ci` ran during any measurement above.

**For these numbers it matters less than it would anywhere else in this
directory, and that is a statement about the claim rather than about the
machine.** The per-cell paired dispersion ran 0.3% to 8.4%; the effects are 5×
to 309× and the growth ratios are 3.4 to 4.0 against a null of 2.0. A stationary
background load is charged to both arms by the palindrome, and it would have to
be wrong by two orders of magnitude, in one arm only, and increasingly so with
input size, to manufacture the curve. It could not have manufactured row four's
1.00 either.

## Consequences

- **This ADR was written in the build phase, when it could claim no measured
  speedup, because handover 26 §6 discards numbers produced there. It carries
  one now** — the Measurement section above, taken in the measurement phase
  against the pair of binaries the build phase staged: `/tmp/praxis-arms/W2-b`
  is this branch, `/tmp/praxis-arms/W2-a` is this branch with the toggle
  reverted. The toggle is one constant, `text::COUNT_IS_CACHED`, driven by the
  `adr115-arm-a` feature on `praxis-runtime`; with it set, the count is never
  remembered and `text_ascii_bytes` always refuses, which is the pre-ADR-115
  complexity with the representation otherwise identical. Arm A is **not**
  `main`: Decision 4 is in both arms, because it is a separate finding and
  leaving it out of the baseline would attribute its win here.
- **The measurement this package deserves is not a benchmark-suite number, and
  the suite confirmed it by not moving.** No `.px` file in `benchmarks/` iterates
  a `Text`; seven of the eight contain no double quote at all; all eight read one
  integer. The suite came out at 0.994× with 8 of 8 deltas unresolvable, which is
  the prediction, not a disappointment. The claim is a complexity claim and it is
  now measured as one: arm A's exponent is 1.92–1.96 where arm B's is 0.98, on
  three separate text shapes.
- **`size_of::<TextPayload>() == 32` is now a build failure to break**, along
  with `OwnedText == 24` and `SourceSlice == 24`. Handover 26 §9 flagged the 32
  as never measured on this tree; it is measured, it is right, and the next
  person to add a field to a text payload will be told the price by the compiler
  instead of by the pacer.
- **`praxis_get_input`'s buffer is now scanned once, by whichever text first
  wants a length.** For a program that parses a 10 MiB input and takes one
  character out of one capture, that is a whole-buffer `is_ascii` pass it did not
  pay before — one linear pass, against a per-call linear pass it *did* pay
  before, so it is behind after the first call and ahead after the second. Making
  it eager instead would charge the same scan to programs that never index at
  all, which is the trade Decision 1 refuses.
- **Four unit tests are `#[cfg(not(feature = "adr115-arm-a"))]`.** They observe
  the cache, which arm A does not have. The tests that state what a `Text`
  *answers* are not gated, and the whole suite passes in both arms (367 and 363
  in `praxis-runtime`); both binaries were also run over all eight benchmarks and
  over a text-heavy program with ASCII and non-ASCII input, with byte-identical
  output.
- **A permanent cargo feature exists that makes the runtime slower on purpose.**
  That is a real cost and the alternative was worse: a toggle edited in and out
  of the source produces an arm A that is not in the tree, and handover 26 §6's
  whole point is that a comparison which cannot say what it held constant is not
  a measurement. It is documented as measurement-only in `Cargo.toml` and nothing
  in the workspace enables it.

## Open questions

- **The loop header should hoist, and ADR-108 already has the machinery.**
  `lower_for` emits the plan's `len` call inside the header block. For `Text` it
  is loop-invariant by the type's immutability, and `b.loop_preheaders` — the
  thing ADR-108 built so a pass would not be needed — is on the stack at exactly
  that point. It is one call per loop instead of one per step, and it removes an
  `Int` box per iteration with it. It is not in this package because
  `praxis-mir/src/build.rs` has other owners this round. For the seven
  non-`Text` in-place plans the same hoist is **not** available without deciding
  what `for x in v { v.push(…) }` means, which ADR-066 left to the snapshot rule
  and did not answer for the three iterables that index themselves.
- **Should `praxis_text_len` return an interned `Int` without allocating?** It is
  `Effect::Allocates` and boxes its answer per call, which after this change is
  the entire remaining cost of a `for c in t` header. ADR-113's
  `emit_inline_intern` is the shape; it is W4b's territory (`praxis_vec_len` is
  the same row for the same reason) and it should take `praxis_text_len` with it.
- **Is `text_str`'s `from_utf8` worth removing from the remaining callers?** It
  is a linear validation of bytes the type already guarantees, and it survives on
  `t[i]`'s multi-byte path, in `text_format` and in `Input::new`. Each is already
  linear in the same bytes for its own reasons, so none of them is a complexity
  bug; it is a constant factor, and deleting it means `from_utf8_unchecked`,
  which ADR-111 rejected in its own context and whose argument carries here.
- **Should a `Slice` of a multi-byte owner be forbidden rather than tolerated?**
  The parser could copy such a capture into an owned payload, which would give
  every text its own count and close row four of Decision 3's table — at the cost
  of the zero-copy property ADR-022 chose the source slice for, on exactly the
  inputs where copying is most expensive. Not obviously wrong, and it would need
  a measurement of how much non-ASCII input the language actually sees before it
  could be argued either way.
