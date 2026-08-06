# ADR-051: The diagnostic-code allocation

**Date:** 2026-07-29
**Status:** Accepted — amended 2026-07-29 for `Y017` (S14/TY-21) and 2026-07-30
for `Y018`, `Y124`, `N006`, `Y019`, `Y020`, `Y021` and `N007` (S24/REP-01, S26/REP-05,
S26/REP-14, S25/REP-08, S25/REP-16) and 2026-07-31 for `N008` (REP-26) and
2026-08-04 to **retire `Y009`** (ADR-125); see the amendment notes under each
table
**Milestone:** Repair (answers the plan's **D13**, which binds before S13)

## Context

Four groups of findings across S13–S19 each need diagnostic codes, and none of
them can be allocated locally: a code is a user-facing, permanent identifier, and
two stages picking `Y009` independently is a collision nobody notices until both
have shipped. The plan asks for one owner to allocate the whole block before S13
starts.

The plan's own inventory of what is already spent is **incomplete**. Checked
against the tree, the taken codes are:

| Category | Taken today |
|---|---|
| `T0xx` Lex | `T001` unterminated block comment · `T002` unterminated backtick template · `T003` unexpected character · `T004` unterminated text literal · `T005` invalid escape |
| `P0xx` Parse | `P001` unexpected token · `P002` missing statement separator (S12/FE-04) |
| `N0xx` Name | `N000` internal: parse root is not a `SOURCE_FILE` · `N001` unresolved name · `N002` unknown type |
| `Y0xx` Type | `Y001` type mismatch · `Y002` infinite type · `Y003` annotation conflict · `Y004` not equatable · `Y005` not iterable · `Y006` not orderable · `Y007` wrong type-argument count · `Y008` duplicate field/variant in a declaration · `Y110` no such method · `Y112` no such field · `Y120` non-exhaustive match · `Y121` unreachable arm |
| `I0xx` Input | `I000` malformed parser expression · `I001` template/AST conversion error · `I010` unknown atomic parser · `I020`–`I022`, `I024`–`I027` validation · `I030` template scan error |

`R0xx` (Runtime) is a declared category with no member. `I023` is the one hole
inside an otherwise contiguous block.

## Decision

The `Y0xx` user block continues from **`Y009`**. `Y09x` is reserved for internal
compiler errors, `Y11x` for member errors (methods, fields), `Y12x` for match
errors — the split the existing numbers already imply. Declaration mistakes go in
the **Name** category, following F2's own sketch, which puts `NameIsNotAType`,
`DuplicateDeclaration` and `NestedFunction` there.

### Name — `N0xx`

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| `N003` | TY-11 | S13 | a name is used in type position but names a value |
| `N004` | TY-24 | S13 | a name is declared twice in one scope |
| `N005` | TY-23 | S13 | a function is declared inside a function |
| `N006` | REP-14 | S26 | a `struct`/`enum` declaration that refers to itself (**amendment**) |
| `N007` | REP-22 | — | a `fn` body naming a binding declared outside it (**amendment**, ADR-068) |
| `N008` | REP-26 | — | a record literal whose head does not name a `struct` (**amendment**) |

**Amendment (2026-07-31).** `N008`, for REP-26. The rule above allocated it: "a
name used in type position that names a value" is `N003`, and a record literal's
head *is* a type position — what is wrong is which declaration the name reaches,
which is a mistake about the name and not a pair of types that failed to unify.
It is not `N003` itself because the kinds that reach it are wider than "a value":
an `enum` is a genuine type and still has no fields to initialize, so the message
names the **kind** rather than claiming the name is not a type. It is emitted in
inference, not at lowering, for `Y019`'s reason — a literal on a non-`struct` head
used to pass `praxis check` and produce a value with no representation (REP-01's
shape). **`N009` is the next free Name code.**

### Type — `Y0xx`, the user block

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| ~~`Y009`~~ | TY-14 | S13 | ~~assignment to something that is not a `var`~~ — **retired**, ADR-125 |
| `Y010` | TY-15 | S13 | a compound assignment whose operands are not numeric |
| `Y011` | TY-20 | S14 | `return` outside a function |
| `Y012` | TY-20 | S14 | `break`/`continue` outside a loop |
| `Y013` | TY-28 | S17 | an integer literal outside the representable range |
| `Y014` | TY-32 | S17 | a type used as a `Map`/`Set` key that cannot be hashed |
| `Y015` | TY-31 | S17 | a type used where a numeric one is required |
| `Y016` | TY-26, TY-27 | S17 | an operator that is not defined for these operand types |
| `Y017` | TY-21 | S14 | a `break` carrying a value out of a `while`/`for` (**amendment**) |
| `Y018` | REP-01 | S24 | a **generic** `fn` used as a value (**amendment**) |
| `Y019` | REP-08 | S25 | a `.n` element access on something with no such element (**amendment**) |
| `Y020` | REP-16 | S25 | a subscript on a type that has none, in either direction (**amendment**) |
| `Y021` | REP-16 | S25 | an assignment whose left side names no storage (**amendment**) |
| `Y023` | REP-47 | — | a backtick parser template written where a value is expected (**amendment**) |

**Amendment (2026-07-30).** Four codes, two of them allocated here and two
**recorded late**. `Y018` (REP-01, S24 — ADR-061) and `Y124` (REP-05, S26 —
`3306a04`) were spent by the sessions that needed them and this file was not
amended at the time, which is the one thing the last consequence below asks for.
They are in the tables above now, so the registry is again what a stage reads
before allocating. Neither collides: both extended a contiguous block.

`Y019`, for REP-08 — a `.n` on a receiver that is not a tuple, or an index past
its arity. Not `Y112` ("no field on this type"): a tuple has no field *names*, so
that message would name the wrong thing, and `Y112` was emitted at **lowering**,
which `praxis check` does not run — `Y019` is emitted in inference for `Y018`'s
reason. (As of REP-28's correction, 2026-07-31, `Y112` has a second emitter in
inference and is the one that fires for a receiver `check` can decide; lowering's
now fires only for callers that lower without checking first. `Y019` is unchanged
— it was right for the reason it was written, and it is right for the same reason
now.) One code with two messages, because "you cannot index this" and "there is
no element there" are one mistake at two receivers, and the arity is the useful
thing to say when there is one.

`N006`, for REP-14. The rule above is what
allocated it: "declaration mistakes go in the **Name** category", and a `struct`
that refers to itself is a mistake about what was *declared* — there is no pair of
types to have failed to unify, which is what a `Y0xx` code is for. `N004` and
`N005` are the precedents — which is what kept REP-14 out of the `Y0xx` block and
left `Y019` for REP-08. ADR-063 is the decision behind it (D17).

`Y020` and `Y021`, for REP-16 (ADR-064). `Y020` is one code with two messages, for
`Y019`'s reason: a receiver with no subscript and a receiver with no *store* are
one mistake at two ends of the same surface — and the two messages have to differ
because a `Vec` reads through `v[0]` and has no element store, so "cannot be
indexed" would be false about it. It also covers the right receiver at the wrong
arity (`grid[x]`, where §6.4 spells it `grid[x, y]`), since arity is part of what
selects the operation.

`Y021` is *not* `Y009` ("assignment to something that is not a `var`"). `Y009` was
about a binding that exists and may not be written; `Y021` is about a left side
that is not a place at all — `f() = 1`, `p.x = 1`. Both were in inference, for
`Y019`'s reason. `Y009` has since been retired (see the amendment below), which
leaves `Y021` as the only thing an assignment's *left side* can be reported for.

**Amendment (2026-08-04, ADR-125).** **`Y009` is retired.** The language no
longer has a binding that cannot be written, so the report has nothing left to
describe. The number is **not** reissued: a code is a permanent user-facing
identifier, and re-spending one is how an old message and a new one come to
answer to the same name. `DiagCode` carries a comment at the hole saying so, and
the `Y0xx` block continues from `Y022` as before.

**Amendment (2026-07-31, REP-47).** `Y023` — a backtick parser template written
where a value is expected. §7.1 says the parser-expression sublanguage is entered
at `read` or at `parse(text, …)` and nowhere else (REP-34 established that
boundary from the other side, for labelled arguments), so a template standing
alone is grammar the design does not write; it used to be typed `Text` and
lowered as a text literal containing its own braces. Emitted in **inference**,
for `Y019`'s reason: `praxis check` must see it. ADR-084 is the decision.

It is `Y023` and not `Y022` because this session's plan reserved the block from
`Y023` upward for its own use, and a gap in a registry costs nothing while two
sessions colliding on one number costs the identifier. The `Y0xx` user block is
otherwise contiguous through `Y021` and the `N0xx` block through `N008`
(REP-26's amendment above).

**Amendment (2026-08-01, D16).** `Y024` — a call whose argument count does not
match the function's. ADR-089 decides that a name has exactly one signature, and
this is the code that says so when one is broken. Before it, `assert(cond, "why")`
and every other arity mistake came back as `Y001` showing two whole function
types to diff by eye — an inference accident to read, sitting beside a `Y007`
that names collection arity and a `Y110` that names method arity.

It is raised from **`TypeDb::unify`**, not from `infer_call`, because that is
where the knowledge already was: the `Func`-vs-`Func` arm compares
`ps_a.len() != ps_b.len()` and discarded the fact. Raising it there means every
function unification benefits, not just direct calls. There is no
`assert`-specific message — a rule stated in four places goes stale in three.

**`Y022` is still free; `N009` is still the next free Name code.**

`TY-20` gets two codes rather than the one the plan lists: "`return` with no
function" and "`break` with no loop" are different mistakes with different fixes,
and a shared code makes the message do the discriminating.

**Amendment (2026-07-29, S14).** `Y017` was not in the original allocation, and
the list below still said TY-21 needed no code. That was right for the finding as
the audit wrote it — "a `loop`'s value is not its `break` value" is a missing
computation, not a missing report — and wrong once **D2** was answered: only a
`loop` is an expression loop, so `while c { break 1 }` is a mistake that has to
be *named*. It is not `Y012`, which is `break` with no loop **at all**: here the
loop exists and it is the kind of loop that is wrong, so a shared code would
again make the message do the discriminating. This is the amendment path the last
consequence below describes, taken rather than argued around.

### Type — `Y09x`, internal

| Code | Stage | Meaning |
|---|---|---|
| `Y099` | S15 | internal: a type the compiler expected was absent (F15's per-node map) |

An internal error is not a user mistake; giving it its own number keeps it out of
the block a user is ever told to look up, and makes "did we emit an internal
error?" a greppable question.

### Type — `Y11x`, member errors

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| `Y113` | HIR-04 | S16 | a record literal is missing one or more fields |
| `Y114` | HIR-04 | S16 | a record literal **or pattern** names a field the type does not have (**amendment**) |
| `Y115` | HIR-04 | S16 | a record literal **or pattern** names one field twice (**amendment**) |

`Y111` stays unallocated: `Y110`/`Y112` were assigned as "method"/"field" with a
gap between them, and closing it retroactively would make the two look like a
range they never were.

### Type — `Y12x`, match errors

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| `Y122` | HIR-07 | S16 | a pattern names a variant the scrutinee's type does not have |
| `Y123` | HIR-06 | S16 | a pattern's shape cannot match the scrutinee's type |
| `Y124` | REP-05 | S26 | a pattern naming more sub-patterns than the variant holds (**amendment**) |
| `Y125` | REP-25, REP-29 | — | a pattern that must match every value but can fail, in a **binding** position (**amendment**) |

**REP-29 spent no code.** A closure parameter is the third binding position — a
`let`, a `for` binding and a parameter — and a pattern that can fail there is the
same mistake `Y125` already names, with the same fix. The message differs by what
it says the pattern has to match: an *item* for a `for`, an *argument* for a
parameter.

**REP-10 spent no code** (ADR-069). A record *pattern* naming a field the record
does not have, or naming one twice, is the literal's own mistake read in the other
direction, so it is `Y114`/`Y115`; a one-element tuple pattern and a record
pattern whose head is not a record are both `Y123`, whose meaning — "this shape
cannot match" — is exactly what is wrong with them. The next free codes are
`Y022`, `Y116`, `Y126` and `N009`.

**ADR-124 spent no code either**, and it narrows two of them. `Vec` and `Deque`
have element stores now, so the receiver `Y020`'s store message describes is
`Text` alone — it reads a `Char` out and is immutable (§4.3), which is the same
"reads but does not write" asymmetry the paragraph above gives as `Vec`'s. And a
**field** left `Y021`: `p.x = 1` is a store into a place, so what is left under
that code is `f() = 1` and `a + b[0] = 1`, and the message says "a name, a field,
or an index". Both features reuse reports that already existed — a field a record
does not have is the read's `Y112`, and `p.x min= 3` is `Y016`, an operator the
type does not have, rather than `Y020`, a subscript nobody wrote.

### Input — `I0xx`

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| `I011` | IP-04 | S19 | an invalid capture name in a template |
| `I012` | IP-06 | S19 | an unknown capture kind |
| `I013` | IP-07 | S19 | an unknown parser constructor |
| `I014` | IP-07 | S19 | a constructor argument that is invalid or in excess |
| `I023` | IP-10 | S19 | an empty separator |
| `I028` | IP-09 | S19 | a misplaced or duplicated **unbounded** `repeated(...)` tail |

`I023` fills the hole rather than extending the block, because it belongs to the
validation family the `I02x` block already holds.

### Findings that need no code

- **TY-12** (collection arity) is `Y007`. S11 already extended `Y007` to nominal
  defs so `Option[Int, Text]` reports there; the plan lists TY-12 separately
  because the audit found the `Option` case separately.
- **TY-16, TY-17, TY-18, TY-19, TY-25** report through `Y001`. Each is a
  unification bug — the check is missing, not the message.
- **TY-13, TY-30, HIR-03, HIR-05, IP-11** are structural or semantic fixes with
  no new report. HIR-05's is a parse error now (FE-02). **TY-21 was on this list
  and is not any more** — see the amendment above: D2's answer turned half of it
  into a report.
- **TY-33, TY-34, RT-14** are blocked on D5, D6 and D1. If D5 deletes the phantom
  prelude names they become `N001`; the others are type changes, not diagnostics.

## Consequences

- **F2 is the enforcement and lands as S13's first foundation commit.** This ADR
  fixes the numbers; `praxis_source::DiagCode` — an exhaustive enum whose
  `code()` is the one place a `(category, number)` pair is written, with
  `DiagnosticCode::new` demoted to `pub(crate)` and a `DiagCode::ALL`
  injectivity test — is what stops the next stage from allocating locally again.
  Until it lands, this file is the registry.
- **`P002` was spent by S12 and is not out of this block.** The plan lists FE-04's
  separator among the codes D13 must allocate; categories are numbered
  independently and the parse category had only `P001` in it.
- **Nothing here renumbers an existing code.** Every allocation is an extension,
  so no message a user has already seen changes its identifier.
- **A stage that discovers it needs a code not listed here amends this file
  first.** That is cheaper than the collision, and the amendment is the record of
  why the finding needed a report the audit did not anticipate.
