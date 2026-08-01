# ADR-090: A `block` item is offered its own lines, and the window is a narrowing rather than a bound

**Date:** 2026-08-01
**Status:** Accepted — implemented
**Milestone:** Repair (REP-58)

## Context

§7.7's own "repeated labeled blocks" example did not run.

```praxis
let monkeys = read sections(
    block(
        `Monkey {id:int}:`,
        `  Starting items: {items:csv(int)}`,
        `  Operation: new = old {operator:char} {operand:word}`,
        `  Test: divisible by {divisor:int}`,
        `    If true: throw to monkey {if_true:int}`,
        `    If false: throw to monkey {if_false:int}`,
    )
)
```

`praxis check` was clean and the type was right — the debugger printed
`Vec[{ id: Int, items: Vec[Int], operator: Char, operand: Text, divisor: Int,
if_true: Int, if_false: Int }]`. Only the parse failed, against the real
AoC-2022-day-11 sample:

```
error: program faulted: input parse mismatch
       at input offset 34..149: expected the rest of the field
       actual:   Starting items: 79, 98⏎  Operation: new = old
```

The offsets say the mechanism exactly. Section 0 is bytes 0..149.
`"Monkey 0:\n"` is 10 bytes and `"  Starting items: "` is 18, so the capture
starts at 28. `csv` splits on commas only: field 0 is `"79"`, and field 1 is
everything from 31 to 149 — `" 98\n  Operation: … monkey 3"`. `int` reads `98`
and stops at 34, and the field's exhaustion check rejects the 115 bytes left
over.

**The measurement that decided it.** The identical template answers differently
depending on which construct holds it:

| spelling | over the same bytes | answer |
|---|---|---|
| ``read lines(`items: {items:csv(int)}`)`` | `"items: 79, 98\nitems: 54, 65, 75, 74\n"` | 2 ints per element |
| ``read block(`items: {items:csv(int)}`, `op: {op:word}`)`` | `"items: 79, 98\nop: plus\n"` | fault at `13..23` |

One template, two answers. That is ADR-078's own defect class, stated in its
Context as "two constructs in one stage disagreeing about one byte".

**Both halves of the mechanism are individually correct.** `walk_template` hands
a capture with no literal after it the rest of its region — ADR-078 Decision 3,
which exists so a root-level template does not fault on the file's trailing
newline. `walk_exact` requires a CSV field to be consumed — ADR-078 Decision 2.
What was missing is the **parent**. ADR-078's thesis is that the window is the
parent's job, and `block` was the one sequencing construct that computed no
window at all:

```rust
let walked = unsafe { walk(rt.ctx, i, plan, *child, region.from(cursor))? };
```

`lines` narrows to a line, `sections` to a section, `csv` to a field,
`ws`/`sep`/`matrix` to a token, `grid` to a cell. `block` handed every item
everything from the cursor to the end of the section, so a capture that was its
template's last part met the unbounded-last-part rule and swallowed the next
five lines.

`block` already *believed* its items were line-anchored. `skip_line_boundary`
exists so item *n+1* starts on the next line, and its comment said "§7.5 block
items are line-anchored"; `walk_block`'s own doc said "a chain of line-anchored
templates advances line by line". Both sentences were true of where an item
*starts* and false of how far it may *reach*. Half a rule.

## The two candidate rules

1. **A block item is offered its own lines.** `block` computes a window for each
   item the way every other sequencing construct does.
2. **Amend §7.7.** The one spelling that works without a code change is an
   explicit terminator on the greedy line —
   `` `  Starting items: {items:csv(int)}\n` `` — so the document would carry a
   trailing `\n` on exactly one of its six lines and say nothing about why.

A third was considered and rejected on evidence: bound every item to a line
*and* require exhaustion. It breaks ``block(`a: {a:int}`, `b: {b:int}`)`` over
`"a: 1 b: 2"`, which works today, and it breaks every named `lines(...)` item —
which is §7.5's own `block` example and two corpus fixtures.

## Decision 1

**A `block` item is offered its own lines: a *template* item gets the line it
starts on plus one more line for each `\n` the template writes; every other item
gets the rest of the region.**

Candidate 2 was rejected because the rule a reader would have to learn is
"append `\n` after a capture whose parser is greedy", where greedy means `csv`,
`ws`, `sep`, `chars`, `text`, `rest`, `lines`, `sections`, `grid`, `matrix` and
not `int`, `uint`, `float`, `byte`, `digit`, `char`, `word`, `identifier`,
`one_of`. That list appears nowhere, is invisible in the type — `{items:csv(int)}`
and `{items:int}` differ only in whether the `\n` is mandatory — and forgetting
it is a fault 115 bytes away from the template that caused it. It would also
ratify the `lines`/`block` disagreement permanently and leave it undocumented.

**The template/non-template split is derived, not tabulated.** §7.2 defines a
template as a description of characters *within a line*, and gives `\n` as the
template's own way of saying it spans another one. So a template already states
its extent and the window merely reads it off. `lines`, `sections`, `grid` and
`matrix` are defined on several lines by their §7.5 entries and compute their
own extent, so bounding them here would be a second, disagreeing opinion about a
question they already answer. Any other split would need a per-constructor "is
this construct single-line?" table — the rule-in-N-places trap ADR-078's own
corollary warns against, and the same objection that sank the third option.

**ADR-072 Decision 1 had already committed to this.** It chose a full parser
expression for a capture body precisely so §7.7's `{items:csv(int)}` would be
legal — "restricting the body would have produced a language that cannot run the
design document's text". Making the body legal and then requiring a punctuation
workaround to use it would undo that decision by other means.

## Decision 2

**The window is a narrowing (`ByteRegion::subregion`) and not a bound
(`walk_exact`): an item may stop short of its window, and `block` carries its
cursor to the next item.**

`block` *sequences*. It needs each item's stopping position the way
`walk_template` needs each part's, which is what makes two items on one line
work: ``block(`a: {a:int}`, `b: {b:int}`)`` over `"a: 1 b: 2"` is 3 on both
sides of this change, and requiring exhaustion would break it. It would also
break every named `lines(...)` item, since `lines` deliberately does not consume
a region's trailing blank run.

ADR-078 Decision 1 names narrowing as the sanctioned mechanism — "a child parser
gets a narrower window on the same buffer" — and Decision 3 says requiring a
region to be filled is a *parent's* decision. Narrowing without requiring
exhaustion is exactly what the two authorise together.

### Where the rule lives

At `block_item_window` in `crates/praxis-runtime/src/parser.rs`, which is the
enforcement site, and nowhere else. `walk_block`'s doc and `skip_line_boundary`'s
doc each used to half-state line-anchoring; they now point at it — where an item
starts is `skip_line_boundary`'s answer, how far it reaches is
`block_item_window`'s — and neither repeats it. The line-finding itself is
`cursor::line_window_end`, which is `split_lines`' notion of a line asked
forwards from a cursor and restates none of the `\r`/terminator rule.
praxis_technical_design.md §7.5's `block(parser...)` entry states it once for the
reader, and §7.7 needs no edit: its example runs as written, so it is the
reader's illustration of the §7.5 rule rather than a second statement of it.

## Consequences

- **A template block item may no longer read past its own line by accident.**
  Three shapes change answer, and the first two change it *silently*:
  - ``read block(`a: {a:text}`)`` over `"a: hello\nworld\n"` was 12 bytes
    (`"hello\nworld\n"`) and is now 5 (`"hello"`).
  - ``read block(`a: {a:rest}`, `b: {b:word}`)`` over `"a: hello\nb: plus\n"`
    faulted (`expected literal "b:"` at 17..19) and is now 5.
  - A final `{w:rest}` item over a last line `"abcd\n"` was 5 bytes and is now 4
    — it no longer keeps the terminator, which is what ``lines(`{w:rest}`)``
    already answered.
  All three move `block` onto `lines`'s answer, which is the point.
- **A *non-template* greedy item followed by another item still swallows.**
  ``read block(`h:`, a: csv(int), b: word)`` over `"h:\n1,2\nfoo\n"` faults
  identically before and after. It is named here rather than fixed: it is a
  **loud** fault and not a wrong answer, no finding is behind it, and bounding it
  needs the per-constructor table Decision 1 rejects. §7.5's block example and
  both corpus fixtures put their `lines(...)` item last, so nothing in the tree
  wants it.
- **No existing test changes answer.** `cargo test --workspace` is green with
  nothing else edited, and these shapes were re-checked by hand against both
  binaries: two block items on one line (3 on both), a `\n`-spanning template
  (6 on both), a block item whose capture is a plain `int`, a
  `sections(block(...))` with an unconsumed line still faulting with `expected
  the rest of the section`, a section with no trailing newline, a CRLF copy of
  the AoC sample answering the same as the LF copy, and
  `tests/aoc-corpus/m9_almanac.px` and `m9_repeated_labeled_blocks.px`.
- **The gates for the *other* half of the rule already existed and are named
  here so they are not "simplified" away.** `m9_block_template_plus_named_field`
  and `m9_block_second_section` in the codegen crate's `jit.rs`, and
  `tests/aoc-corpus/m9_repeated_labeled_blocks.px` and `m9_almanac.px`, all carry
  a named `lines(...)` item that must keep the rest of its region. They go red
  the moment someone line-bounds *every* item rather than only templates —
  `tests/input-parsers/block_item_window.px` asserts that half too, in the same
  file as the pair, so the two halves cannot drift apart.
- **The `\n` count is load-bearing and is gated.** Forcing `extra` to 0 — a
  window of exactly one line, always — faults `at input offset 3..3: expected
  whitespace` on ``block(`{x:int},{y:int}\n{z:int}`, …)``, because the
  template's own `\n` part has no terminator left inside its window. Observed
  through `tests/input-parsers/block_item_window.px`;
  `m9_block_two_template_fields_flatten` in `jit.rs` carries the same template
  and goes red the same way.
- **No ABI surface.** `ParserPlan`, `PlanNode` and `BlockItemNode` are host-side
  leaked data and only the plan id crosses `extern "C"`. No plan-shape change, no
  descriptor change, no type-synthesis change, so `praxis check` output is
  byte-identical and `RUNTIME_ABI_VERSION` is untouched.
