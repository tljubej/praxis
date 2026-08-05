# ADR-086: A `Text` subscript answers a `Char`, and two conversions are the whole `Char` surface

**Date:** 2026-08-01
**Status:** accepted — implemented
**Milestone:** Repair (register REP-65)
**Supersedes:** nothing. Amends §4.13, which said nothing about indexing.

## Context

```praxis
fn main() {
    var b = "sddddd"
    out(b[4])
}
```

printed `100`. So did the same program with a different character at a different
index, which is what made it look like indexing was broken: `100` is `d`, and
index 4 of `"sddzdd"` is also `d`. Indexing was right. **The type was wrong.**

Two sites agreed with each other and with nothing else:

```rust
// crates/praxis-stdlib/src/builtins.rs — text_index() and text_get()
result: TypePattern::Scalar(ScalarType::Int),
```

```rust
// crates/praxis-runtime/src/abi.rs — praxis_text_get
// Return the scalar value as an Int (Char is reserved; M5 uses Int).
unsafe { gc_alloc(ctx, scalars::INT_PAYLOAD, ch as i64) }
```

**The comment names the cause, and the cause expired.** `Char` was reserved in
M5. It has not been since M6: `resolve.rs`'s own doc comment says "`Char` is
wired end-to-end" and lists it in `KNOWN_TYPE_NAMES`, §4.3's table gives it a
payload ("Unicode scalar payload / validated scalar value"), §4.4 writes
`Grid[Char]`, and `scalars.rs` carries a complete `CHAR` descriptor whose
`char_format` renders the character. The M5 shortcut simply outlived M5.

So one method catalog had `Grid[Char]`'s `[]` answering a `Char` and `Text`'s
`[]` answering an `Int` — the language contradicting itself inside the single
mechanism ADR-064 chose *because* it is single. `read grid(char)` produced cells
that print as characters; `t[i]` produced numbers.

**Nothing in the design doc pinned the `Int`.** §4.13 ("Text behavior") says
nothing about indexing, §5.7's method table is illustrative and does not list
`Text.get`, and Appendices A and B are silent. The only statements of the old
rule were the two catalog doc strings and the runtime comment — the
implementation was its own only authority, which is the situation this repair
round exists to end.

## Decision

### 1. `t[i]` and `t.get(i)` answer a `Char`

The two spellings are one row's answer and lower through one wrapper, unlike
`Map`'s two reads, which differ by design (§4.7).

This is **ADR-083 one level down.** There, a value whose type the language *has*
was rendered as a type it is not, and the fix was to make the output match the
type rather than teach the reader to supply context. Here the *static type* is
wrong, not only the rendering: `out(b[4])` printing `100` is `out(1.0)` printing
`1` with the mistake moved from the formatter into the catalog. ADR-083's reason
for rejecting "let the context say what it is" applies verbatim — §16.3 makes the
rendered form load-bearing as a **sort key** for `Map`, `Set` and `Counter`, so
the rendering is an answer and not only a printing.

### 2. Exactly two conversion rows: `Char.to_int()` and `Int.to_char()`

There was no `Char` receiver row in the catalog at all. Two are added and no
more.

**`Char.to_int()` is required, not a nicety.** The `Int` result *was* the entire
capability surface for a text character: `t[i] - 48` to read a digit, `t[i] >= 97`
to test a range, a `Map[Int, _]` keyed on a character. `capability::supports_numeric`
excludes `Char` on purpose — "a `Char` is a scalar value and not an arithmetic
one" — so without this row decision 1 would be a straight regression of a real
AoC idiom. With it, every program expressible before stays expressible by
inserting `.to_int()`, which is the bar a type correction has to clear.

**`Int.to_char()` because §4.12's precedent is explicitly a pair.**
`Float.to_int()`/`Int.to_float()` are written as two halves of one conversion.
A one-way conversion is a one-way door: with `to_int` alone a program could take
a `Char` apart and never build one, leaving `Grid[Char]`, `Vec[Char]` and
`Map[Char, _]` write-only from the language's side. It is the **narrowing** half
and therefore the one that faults — `Float.to_int()`'s role exactly — because not
every `Int` is a Unicode scalar value. It owes no new fault kind: `InvalidChar`
and its range check already exist for `praxis_alloc_char` (RT-18), and the two
doors now share one `checked_alloc_char` rather than each stating the rule.

### 3. The char literal is deferred as **D19**, not smuggled in

The language has no `'a'`; `let c = 'a'` is `T003`. Decision 1 makes that
survivable rather than blocking, because **it supplies a spelling out of syntax
that already exists**: `"#"[0]` names a `Char`, and `" "[0]` names a space.

Adding a literal here would decide a real language question — the escape set,
whether a one-scalar `Text` should coerce instead, what `'` costs elsewhere in
the grammar — as a side effect of a type fix. That is precisely what the register
warns about under D16: answering the cheapest motivating case in isolation "sets
the precedent by accident."

Worth recording so whoever takes D19 does not re-derive it: **everything below
the parser is already built and currently dead.** `Lit::Char(u32)` exists in the
HIR and *nothing constructs it* — its only readers are exhaustive-match arms —
and `AllocKind::Char`'s MIR lowering and its Cranelift codegen are complete. D19
is a lexer, a parser arm, and `lower_lit`. Nothing else.

### Why not `Char.to_text()`, and why not the character-class family

- **`Char.to_text()`** is one third of a gap §4.13 records in the design doc's
  own words: "building a `Text` out of a number is not yet possible in any
  spelling." Answering that for the one type that did not ask leaves the other
  two open and creates a *second* spelling for "is this character a `#`"
  (`t[i].to_text() == "#"` beside `t[i] == "#"[0]`). Two spellings for one
  question is what ADR-077 refused for accessors. The `to_text` family is one
  decision and wants taking whole.

  **The quoted sentence is no longer true as written**, and the argument above
  survives it. `Float.to_text()` exists and prints `5.0`; `Int.to_text()` still
  reports `Y110`, and §8.1's interpolation is still specified and unimplemented.
  So the family is two thirds open rather than three, and the reason for taking
  it whole is unchanged. (The traffic in the *other* direction was answered
  separately: `Text.int()`/`Text.float()` are ADR-136, and they are not part of
  this family — they read a number out of a text rather than writing one into
  it.)
- **`is_digit`, `is_alpha`, `to_upper`, `to_lower`** have no design-doc surface
  asking for them, and `to_int()` expresses every one. Inventing four rows here
  is exactly what REP-46 refused to do with `wrapping_sub`/`wrapping_mul`.

Both omissions are recorded in a block comment above the two rows in
`builtins.rs`, not only here — the convention REP-46 set, so a reader meets the
reason where they meet the gap.

## Consequences

- **The two halves are one commit.** With the catalog changed and the runtime
  not, `compare_kind` routes a `Char`-typed value to `praxis_char_load`, whose
  `read_scalar` answers `None` against the `INT` descriptor and takes
  `scalar_type_mismatch`'s defined panic. A partial tree does not answer wrongly
  — it aborts. That is a landmine for a split commit and a gift to the gates:
  `a_char_and_its_code_point_convert_both_ways` is red in a *different way* for
  each half removed.
- **Three tests pinned the old answer and are rewritten, not deleted** (§8.2).
  `text_len_and_get_end_to_end` and `text_get_indexes_by_scalar_not_byte`
  asserted `101` and `233` off `s.get(1)`; both keep their subjects — the second's
  subject, scalar-not-byte indexing, is load-bearing — and observe them through
  `.to_int()`. `a_subscript_reads_and_writes_through_the_wrapper_its_receiver_needs`
  read `"abc"[1]` as `98` in an `Int`-returning `main`.
- **The corpus needed no expected-output change.** No `.px` under `tests/`
  subscripts or `.get`s a `Text`; every `.get(` there is on a `Vec` or a `Grid`.
- **`Grid[Char]` becomes usable.** `g[x, y] == "#"[0]` type-checks, where
  `g[x, y] == "#"` was `Y001`. The workaround in
  `tests/input-parsers/grid_char_space_cell.px` — comparing a cell to *another
  cell* through `find_all` because no character could be written — is no longer
  forced.
- **No ABI bump.** Two symbols join the manifest and two arms join the exhaustive
  `symbol_address` match, which is what REP-46's trio did. `RUNTIME_ABI_VERSION`
  is about `#[repr(C)]` layout (ADR-075) and no such type changed.
- **`Char` ordering and rendering were already right and are untouched.**
  `CHAR.compare` is `u32` code-point order (ADR-045); containers order by the
  rendered form, and UTF-8 byte order *is* code-point order, so the two agree.
- Two observations recorded rather than fixed, neither caused by this row:
  a `Vec[Char]` of `a` and a `Vec[Text]` of `"a"` both print `[a]` (ADR-083's
  round-trip rule cannot apply to a type with no literal to read back into —
  it becomes decidable only under D19); and `for c in text` is `Y005` because
  `capability::iter_item` answers `None` for a scalar, which reads the same
  before and after this change.
