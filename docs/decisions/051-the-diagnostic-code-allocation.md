# ADR-051: The diagnostic-code allocation

**Date:** 2026-07-29
**Status:** Accepted — amended 2026-07-29 for `Y017` (S14/TY-21; see the
amendment note under the `Y0xx` table)
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

### Type — `Y0xx`, the user block

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| `Y009` | TY-14 | S13 | assignment to something that is not a `var` |
| `Y010` | TY-15 | S13 | a compound assignment whose operands are not numeric |
| `Y011` | TY-20 | S14 | `return` outside a function |
| `Y012` | TY-20 | S14 | `break`/`continue` outside a loop |
| `Y013` | TY-28 | S17 | an integer literal outside the representable range |
| `Y014` | TY-32 | S17 | a type used as a `Map`/`Set` key that cannot be hashed |
| `Y015` | TY-31 | S17 | a type used where a numeric one is required |
| `Y016` | TY-26, TY-27 | S17 | an operator that is not defined for these operand types |
| `Y017` | TY-21 | S14 | a `break` carrying a value out of a `while`/`for` (**amendment**) |

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
| `Y114` | HIR-04 | S16 | a record literal names a field the type does not have |
| `Y115` | HIR-04 | S16 | a record literal names one field twice |

`Y111` stays unallocated: `Y110`/`Y112` were assigned as "method"/"field" with a
gap between them, and closing it retroactively would make the two look like a
range they never were.

### Type — `Y12x`, match errors

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| `Y122` | HIR-07 | S16 | a pattern names a variant the scrutinee's type does not have |
| `Y123` | HIR-06 | S16 | a pattern's shape cannot match the scrutinee's type |

### Input — `I0xx`

| Code | Finding | Stage | Meaning |
|---|---|---|---|
| `I011` | IP-04 | S19 | an invalid capture name in a template |
| `I012` | IP-06 | S19 | an unknown capture kind |
| `I013` | IP-07 | S19 | an unknown parser constructor |
| `I014` | IP-07 | S19 | a constructor argument that is invalid or in excess |
| `I023` | IP-10 | S19 | an empty separator |
| `I028` | IP-09 | S19 | a misplaced or repeated `repeated(...)` tail |

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
