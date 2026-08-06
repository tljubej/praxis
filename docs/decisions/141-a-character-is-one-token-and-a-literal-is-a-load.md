# ADR-141: A character is one token, and a literal is a load

**Date:** 2026-08-06
**Status:** accepted — implemented
**Milestone:** handover 31 item 12
**Supersedes:** [ADR-107](./107-a-small-char-is-one-object-and-there-is-no-char-literal.md)
Decision 2 ("There is no `GcConst::Char`, because there is no character
literal") in its entirety, and
[ADR-086](./086-a-text-subscript-answers-a-char.md) Decision 3's deferral of the
literal as **D19**.
**Amends:** ADR-107's `small_char` consequence bullet (the "no `MIN`, no
`STRIDE`, one consumer crate, no backend reader" paragraph), and
[ADR-100](./100-a-small-int-is-one-object-and-a-literal-is-a-load.md) Decision 4
— *"a literal is a load"* — extended to its second scalar.

## Context

A `Char` is a first-class scalar type with a full method row, an interned
table, its own `ScalarKind` and its own descriptor, and until now no way to
write one down. The spelling was a one-character `Text` subscripted at zero:

```praxis
var splitter = "^"[0]
var hash = "#"[0]
```

Three AoC solves (handover 31) put that spelling in front of a real workload,
and it costs more than the extra four characters:

- **You cannot `match` on a `Char`.** A pattern must be a literal, `"#"` in a
  pattern is a `Text`, and there is no third thing to write — so dispatching on
  a grid cell, the most common single operation in the problem domain, has no
  `match` form at all. It is an `if`/`else if` chain against pre-bound
  variables, which no exhaustiveness check can help with, in a compiler whose
  M12 headline was `match` coverage
  ([ADR-130](./130-a-matchs-coverage-is-analysis-answer-and-the-pattern-is-built-once.md)).
- **Three ways the workaround goes wrong are all `check`-clean.** `"##"[0]` is
  a typo that quietly means the first character and is never diagnosed;
  `""[0]` and `"#"[1]` are index faults at run time.
- **It is a call, not a constant.** `"#"[0]` lowers to `praxis_text_get` and is
  folded by nothing, so it re-evaluates every iteration. Both solves that used
  it hoisted it to a `var` by hand — a thing the author has to know to do, and
  which the source gives no hint about.

ADR-107 anticipated this day and said so in as many words:

> If character-literal syntax is ever added, a `GcConst::Char` becomes correct
> on the same day and `small_char::index_of` is already the compile-time
> predicate it would ask — which is why that function is a `const fn` even
> though only the runtime calls it today.

Every clause of that held. What did **not** hold is ADR-086 Decision 3's
scoping of the same work — "D19 is a lexer, a parser arm, and `lower_lit`.
Nothing else." — which is the subject of Decision 4 below.

## Decision

### 1. The spelling is `'#'`: one Unicode scalar, a text literal's escapes plus `\'`

```praxis
var wall = '#'
var newline = '\n'
var quote = '\''
var accent = 'é'          // one character, not two bytes
```

The body is exactly one Unicode scalar value. The escapes are
`\n \r \t \0 \\ \"` — the set a `"…"` already has — plus `\'`, which a text
literal has no need for. There is **no** `\x` and no `\u{…}`, here because
there is none there.

That last clause is the decision, and it is a decision about drift rather than
about convenience. `praxis_syntax::literal`'s module doc exists because the
workspace once had two text decoders that disagreed (IP-08), and `sep("\t",
int)` split on the two characters `\` and `t` as a result. A language with two
escape tables has two answers to what `\n` is, and the second answer is always
found later and somewhere worse. So there is one table — `decode_escape` — and
both literal spellings read it. Adding `\u{…}` is a change to *the* escape set,
which is a decision worth taking on its own terms and not a side effect of
adding a second literal.

`decode_char_literal` lives beside it, and is called by three passes: the
**lexer**, to decide the length rule; `lower_literal`, to build the expression;
and `PatternBuilder::build`, to build the pattern. Three callers, one decoder —
so "how many characters is this" and "which character is this" are the same
walk. An implementation that counted bytes in the lexer would report `'é'` as
two characters and then lower it correctly, and the disagreement would be a
diagnostic nobody could reproduce.

### 2. `''`, `'ab'` and `'a` are **lexical** errors, which is where `"##"[0]` gets closed

| Written | Before | Now |
|---|---|---|
| `"##"[0]` / `'##'` | silently the first character, no diagnostic ever | `T007`, with a machine-applicable rewrite to `"##"` |
| `""[0]` / `''` | `index out of bounds` at run time | `T007` |
| `'a` | — | `T006`, once, with no cascade |

Two new codes, and `T005` (invalid escape) is shared with the text literal
because it is the same mistake — one code, two messages, the way `Y020` already
carries three.

The reason to put all three at the **lexer** rather than downstream is that a
front-end refusal is the only kind that cannot be reasoned around. A `Char`
that is "the first character of `##`" is a well-typed value; once it exists,
every later pass is right to accept it. The literal's job here is not to be
shorter than `"#"[0]` — it is to make the mistake unwritable, and only the
token boundary can do that.

`T007`'s `'ab'` message carries the fix as a replacement rather than a `help:`
line ([ADR-132](./132-a-code-action-is-a-diagnostics-machine-applicable-suggestion.md)),
because it is mechanical and it is almost certainly what the author meant.

There is a second, quieter win in this decision: a `'` used to be `T003`, so
`var c = 'a'` was **seven** diagnostics — two unknown characters, two `P001`s,
two `P002`s and an `N001` for the `a` in between. It is one now.

### 3. `GcConst::Char`, and the honest asymmetry above U+007F

An in-range literal is `Inst::ConstGc { GcConst::Char }`: the backend loads
`ctx.small_chars` out of the context at a fixed offset, then loads the element
at a compile-time displacement. **Two loads**, which is ADR-100 Decision 4
applied to its second scalar, and `forward`'s fold then turns the *reload* of
that box into an immediate — so `if c == '#'` inside a loop compares against a
baked-in `35` with nothing reloaded.

This half is not polish. Without it, `'#'` takes `lower_lit_gc`'s existing path
— `ConstInt` + `Alloc { AllocKind::Char }` + the `CheckFault` that
`AllocatesAndFaults` forces — which is a call, a guard and a spill where
`"#"[0]` was one call. The syntax alone would have made the language *slower*
at the thing the syntax was added for. The two halves are one change.

`small_char` interns `0..=127`, so a literal above U+007F gets no constant:
`'é'` allocates, and carries a `CheckFault` that **cannot fire**, since the
lexer decoded a real `char` to get here. That is 41-sites-worth of ADR-111's
complaint at a much smaller scale, and it is accepted rather than cured. The
cure is ADR-111's: split `praxis_alloc_char` into a validating entry for the
one untrusted caller (`Int.to_char()`, whose scalar arrives from a program at
run time — [ADR-086](./086-a-text-subscript-answers-a-char.md)) and a
non-faulting one for the compiler's own bytes. It is named here so that whoever
measures it finds the shape already described, and so that nobody re-derives
it. At two instructions on a path that also allocates, it is not worth a
manifest split today.

`GcConst::small_char` is the only way to obtain the constant, for
`GcConst::small_int`'s reason: the backend's `index_of(..).expect(..)` is a
documented agreement between two functions, not a guard, and a lowering site
that decided for itself that its character "is obviously small" would emit a
table read for a slot that does not exist. The ceiling here is 127, low enough
that a program routinely names a code point past it, which makes the discipline
matter more than it did for `Int`, not less.

### 4. The pattern-test arm was an unconditional match, and the scoping that hid it

`lower_pattern_test`'s `Lit::Char` arm read, in full:

```rust
Lit::Char(_) => {
    // Char patterns aren't produced by the parser today; treat
    // as a match (defensive).
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: on_success };
}
```

That is an unconditional jump to success: **every `Char` pattern matched every
scrutinee**. It was unreachable while no character literal could be written, and
the moment one could, `match c { '#' => 1, '.' => 2, _ => 3 }` would have
compiled, coverage-checked clean, verified clean, and returned `1` for every
character. No pass below the JIT can see it.

This is recorded as a decision rather than a bug note because it is the
strongest argument in the tree for a rule: **a defensive arm that jumps to
success is not defensive.** An unreachable arm has to answer the question it
was given or refuse to; "treat as a match" answers a *different* question, and
does it in the shape that produces a wrong answer instead of a crash. The arm
belongs where it now is — folded into `Lit::Int | Lit::Bool | Lit::Char`,
selecting `ScalarKind::Char` — which is the same extract-then-`IntCmp` that
`compare_kind`'s `CompareVia::Scalar(Char)` already emitted for `c == '#'`. Two
spellings, one comparison.

It is also worth naming *why* it survived four documents. ADR-086 Decision 3
closed with "D19 is a lexer, a parser arm, and `lower_lit`. Nothing else." That
sentence was short by three things: `GcConst::Char` and its codegen arm (without
which the feature is a regression, per Decision 3), and this stub. It was
copied forward into the handover verbatim, which is how an under-scoping
becomes a plan. An ADR that under-scopes a future change is exactly what the
next under-scoping will look like.

### 5. No ABI bump

`small_chars` already exists in `RuntimeContext` at its current offset, and no
`#[repr(C)]` layout changes. What changed is that generated code now *reads*
that base, which is not an ABI event — the field's own doc said "generated code
never reads this one", and that sentence describing a fact rather than a
contract is exactly why it could stop being true without breaking anything.

Contrast ADR-107 itself, which **appended** the field and did owe v19. Stated
explicitly so that nobody bumps `RUNTIME_ABI_VERSION` reflexively on reading a
diff that touches `context.rs`.

### 6. `Char`'s rendering is **not** changed, and that is D19's other half answered

`out('a')` prints `a`, and not `'a'`. ADR-086's last consequence bullet says the
`Vec[Char]`-versus-`Vec[Text]` round-trip question "becomes decidable only under
D19"; it is decidable now, and the answer is *no change*.

The reason is not conservatism. §16.3 makes the **rendered form** a load-bearing
sort key for `Map`, `Set` and `Counter`, so changing how a `Char` renders
silently reorders every keyed collection over `Char` — a wrong answer with no
symptom, in the same class handover 31 item 5 exists to remove for `Int`. A
change to `Char`'s rendering is a change to collection ordering wearing a
display question's clothes, and it would have to be argued as one.

The witness renderer is the one place the two spellings do differ, and it was
already right: `LitKey::render` has printed a `Char` witness as `'x'` since
ADR-130, years ahead of the language having that spelling. A `Y120`'s suggested
arm is now a program the reader can paste.

## Consequences

- **The three gates hold.** `match c { '#' => …, '.' => … }` compiles and is
  coverage-checked; `'#'` in a loop is two loads and an immediate compare rather
  than a runtime call; `""[0]`'s and `"##"[0]`'s failure modes stop being
  expressible as a literal.

- **Six dead `Lit::Char` match arms became live**, which is the thing to
  remember about this change: the feature was almost entirely *downstream* code
  that had never run. Two of the six were wrong (`build.rs`'s pattern test, and
  `pattern.rs`'s `Lit::Char(_) => scrutinee_ty`, which typed a char pattern as
  whatever it was asked about, so `match n { 'a' => … }` over an `Int` would have
  type-checked). One in three of the arms waiting for this literal was a defect
  waiting for it too.

- **`exhaustive.rs` needed no change at all**, and that is worth recording as a
  success rather than passing over: `LitKey::Char` already keyed the pattern,
  `signature()` already falls `Char` to `Signature::Open` — so a `match` over a
  `Char` is never exhaustive without `_`, exactly like `Int` — and the witness
  already rendered as `'x'`. Three tests in `coverage_tests.rs` hold that,
  because there is no code here for a regression to break.

- **`small_char::index_of` finally has the compile-time caller ADR-107 predicted
  it would**, thirty-four ADRs later, and the `const fn` was worth keeping.
  `SMALL_CHAR_STRIDE` now exists, which ADR-107's consequence bullet said it
  would not; the premise it argued from — no backend reader — is what this
  decision falsifies, and the module doc records the reversal beside the
  constant.

- **`"#"[0]` still works and is still necessary.** It is how a program gets a
  character out of a *variable* text, which is most of what a program does with
  one. The literal replaces it only where the character is known at the point of
  writing.

- **The corpus needed no escaping rule.** Every `'` in `tests/`,
  `docs/book/examples/` and `benchmarks/` before this change was inside a `//`
  comment, a `"…"` literal or a backtick template — all three consumed whole
  before `classify` is reached — because a top-level `'` had always been `T003`.
  Claiming the token regressed nothing.
