# Numbers

`Int` is a signed 64-bit integer and every arithmetic operator on it is
**checked**: an overflow is a fault that stops the program in
[the crash debugger](../debugger/faults.md), not a wrap that keeps going with a
number nobody wrote. `Float` is IEEE-754 binary64 and behaves the way IEEE-754
says, including never faulting.

The two do not mix. A literal is typed by its syntax — `42` is an `Int`, `42.0`
is a `Float` — and there is no implicit widening in either direction:

```praxis
// There is no implicit widening: an Int and a Float never mix.
var scale = 2
out(scale * 1.5)
```

```console
$ praxis check mixing-numbers.px --color never
error[Y001]: expected Float, found Int

  mixing-numbers.px:3:5
  3 | out(scale * 1.5)
    |     ^^^^^ expected Float, found Int

praxis: 1 error(s)
```

The conversions are explicit and are a matched pair: `Int.to_float()` always
succeeds, and `Float.to_int()` truncates toward zero and faults on anything it
cannot represent. Both types also render: `Int.to_text()` and `Float.to_text()`
each answer exactly the characters `out` writes, because the method and the
printer share one renderer
([ADR-143](../../../decisions/143-the-to-text-family-is-int-float-and-char.md)). The rule that decides an operation's type is the operands':
one `Float` operand makes the operation `Float`, otherwise it is `Int`. (A
`Text` operand makes it `Text` — see [Text and Char](text.md).)

## Overflow is a fault

```praxis
fn double(n) {
    n * 2
}

out(double(4611686018427387904))
```

```console
$ praxis run overflow.px --debug never
error: program faulted: integer overflow

Backtrace:
#0   double
#1   <entry>

  locals:
    n: Int = 4611686018427387904
  temps:
    <tmp#2: Int> @ "2" = 2
    <tmp#3: Int> @ "n * 2" = <uninit>
```

`+`, `-`, `*` and unary `-` all check. So do `/` and `%`, on their one
overflowing case: the smallest `Int` over `-1` has no positive counterpart, and
both fault with the same `integer overflow` rather than the `division by zero`
below. The `abs` prelude helper faults there too, for the same reason.

An `Int` literal outside the 64-bit range never gets that far — it is `Y013` at
compile time, described in [Scalars](scalars.md#literals).

## Opting out of the check

Three modes over the three operators that can overflow without a divisor. Nine
methods, all on `Int`:

| | `add` | `sub` | `mul` |
|---|---|---|---|
| `wrapping_` | `wrapping_add` | `wrapping_sub` | `wrapping_mul` |
| `saturating_` | `saturating_add` | `saturating_sub` | `saturating_mul` |
| `checked_` | `checked_add` | `checked_sub` | `checked_mul` |

```praxis
// The three modes over the three operators that overflow without a divisor.
var big = 9223372036854775807
out(big.wrapping_add(1))
out(big.saturating_add(1))
out(big.checked_add(1))
out(big.checked_mul(2))
out(5.checked_add(1))
out(2.wrapping_mul(big))
```

```text
-9223372036854775808
9223372036854775807
None
None
Some(6)
-2
```

`checked_*` answers a real `Option[Int]`, so a miss is something you
[match on](pattern-matching.md) rather than a sentinel value to remember.

There is deliberately **no** `wrapping_div`, `checked_rem`, `wrapping_neg` or
`checked_abs`. Division by zero always faults, so a `checked_div` answering
`None` would contradict that; `0.wrapping_sub(x)` and `0.checked_sub(x)` already
spell the negation. Praxis has no bitwise operators at all, which is why
`wrapping_mul` exists: modular multiplication has no other spelling.

## Division and modulo

`/` on two `Int`s is integer division truncating toward zero, and `%` is the
remainder with the sign of the dividend: `-7 / 2` is `-3` and `-7 % 2` is `-1`.

Both fault when the divisor is zero, and both report it the same way:

```praxis
fn share(total, parts) {
    total / parts
}

out(share(10, 0))
```

```console
$ praxis run divide-by-zero.px --debug never
error: program faulted: division by zero

Backtrace:
#0   share
#1   <entry>

  locals:
    total: Int = 10
    parts: Int = 0
  temps:
    <tmp#3: Int> @ "total / parts" = <uninit>
```

```praxis
fn wrap(n, m) {
    n % m
}

out(wrap(10, 0))
```

```console
$ praxis run modulo-by-zero.px --debug never
error: program faulted: division by zero

Backtrace:
#0   wrap
#1   <entry>

  locals:
    n: Int = 10
    m: Int = 0
  temps:
    <tmp#3: Int> @ "n % m" = <uninit>
```

Float division does **not** fault. `1.0 / 0.0` is `inf`, `-1.0 / 0.0` is `-inf`
and `0.0 / 0.0` is `NaN`, exactly as IEEE-754 requires.

## Float

Float arithmetic never faults. `%` is not defined for `Float` at all — there is
no float remainder to lower it to, so it is refused at check time rather than
computing something else:

```praxis
// `%` is defined for Int only.
out(5.0 % 2.0)
```

```console
$ praxis check float-remainder.px --color never
error[Y016]: `%` is not defined for `Float`

  float-remainder.px:2:5
  2 | out(5.0 % 2.0)
    |     ^^^^^^^^^ `%` is not defined for `Float`

praxis: 1 error(s)
```

Unary `-` on a `Float` is IEEE-754 negation — the sign bit flipped, nothing else
— so `-0.0` is a value distinct from `0.0`, even though the two compare equal.

`Float` carries twelve methods: `abs`, `sqrt`, `floor`, `ceil`, `round`,
`sign`, `is_nan`, `is_infinite`, `min(other)`, `max(other)`, `to_int` and
`to_text`. `round` rounds half away from zero. `min`/`max` return the *other*
operand when one is `NaN`. `pi()` and `e()` are prelude functions, not methods.
`to_int` is the only one that faults: on `NaN`, on `±inf`, and on a finite value
outside the signed 64-bit range, with `float-to-int conversion out of range`.

### How a Float prints

`out()` and `to_text()` render a finite `Float` in the shortest text that reads
back as **the same `Float`** — one function, called from both, so the pair
cannot come apart. Because `1` is an `Int` literal in this language
and the two types never mix, a whole-numbered float keeps a fractional part:

```praxis
// A Float prints in the shortest form that reads back as the same Float,
// so a whole-numbered one keeps its fractional part.
out(1.0)
out(2.5)
out(1e10)
out(0.1 + 0.2)
out(-0.0)
out(1.0 / 0.0)
out(-1.0 / 0.0)
out(0.0 / 0.0)
out(16.0.sqrt())
out(1.5.to_text())
```

```text
1.0
2.5
10000000000.0
0.30000000000000004
-0.0
inf
-inf
NaN
4.0
1.5
```

There is no exponent notation on output: `1e10` prints its ten zeros and then
takes a `.0` like any other whole number. The three non-finite values print as
`inf`, `-inf` and `NaN` and take no suffix, because they are not decimal
literals. The reasoning, and the defect where `out(1.0)` used to print `1`, is
[ADR-083](../../../decisions/083-a-float-prints-as-a-float.md).

The rendered form is an answer and nothing more. `Map`, `Set` and `Counter` used
to order their entries by it, which put `10.25` between `1.5` and `2.0`; they now
order by the number, so a `Set[Float]` prints `{1.5, 2.0, 10.25}`
([ADR-138](../../../decisions/138-a-container-orders-by-the-value-and-not-by-its-printing.md)).

### NaN

`NaN` is unordered. `==`, `<`, `>`, `<=` and `>=` follow IEEE-754, which means
every one of them is `false` against a `NaN` — including `NaN == NaN`, and
including `NaN <= NaN`. `!=` is the mirror of `==`, so it is the one that
answers `true`:

```praxis
// NaN is unordered: `==` and the four order comparisons are false against it.
var nan = 0.0 / 0.0
out(nan == nan)
out(nan != nan)
out(nan < 1.0)
out(nan > 1.0)
out(nan <= nan)
out(nan.is_nan())
// The two zeros compare equal and print differently.
out(0.0 == -0.0)
out(1.0 / 0.0 == 1.0 / -0.0)
```

```text
false
true
false
false
false
true
true
false
```

`is_nan()` is how you actually test for one.

Inside a container the answer differs, deliberately. A heap or a sort needs a
*total* order or it breaks its own invariants, so the ordering a container
imposes places `NaN` after every number and ties it with itself, and treats
`-0.0` and `0.0` as one key. Source-level `<` is untouched. Both halves, and why
`f64::total_cmp` was rejected, are in
[ADR-045](../../../decisions/045-ordering-semantics-and-the-compare-callback.md).

## The operators, and what binds tighter

This is the whole set. There are no bitwise operators, no exponent operator, no
increment or decrement, and no ternary conditional — `if` is an expression, so
it does that job.

Tightest first:

| Operators | Kind | Assoc. | Notes |
|---|---|---|---|
| `f(x)` &nbsp; `x[i]` &nbsp; `x.name` &nbsp; `x.name(...)` &nbsp; `x.0` | postfix | left | a `(` or `[` continues the expression before it only on the same line |
| `read` | prefix | — | its body is a [parser expression](../input/read.md) |
| `-` &nbsp; `!` | prefix | — | `-` negates a number, `!` negates a `Bool` |
| `*` &nbsp; `/` &nbsp; `%` | infix | left | `%` is `Int` only |
| `+` &nbsp; `-` | infix | left | `+` also concatenates `Text` |
| `==` &nbsp; `!=` &nbsp; `<` &nbsp; `>` &nbsp; `<=` &nbsp; `>=` | infix | left | result is `Bool` |
| `&&` | infix | left | short-circuits |
| `..` &nbsp; `..=` | infix | left | builds a `Range` |
| `\|\|` | infix | left | short-circuits |

```praxis
// Tightest first: postfix, then prefix, then the binary levels.
out(-1.5.abs())       // -(1.5.abs())
out((-1.5).abs())     // the other reading, spelled out
out(-2 * 3)           // (-2) * 3
out(2 + 3 * 4)        // 2 + (3 * 4)
out(2 - 3 - 4)        // (2 - 3) - 4
out(1 + 2 == 3)       // (1 + 2) == 3
out(1 == 1 && 2 == 3) // (1 == 1) && (2 == 3)
out(true || false && false)

var span = 0..3 - 1   // 0..(3 - 1)
var n = 0
for i in span { n += 1 }
out(n)
```

```text
-1.5
1.5
-6
14
-5
true
false
true
2
```

Two of those rows are worth staring at.

**A postfix chain binds tighter than a prefix `-`.** `-1.5.abs()` is
`-(1.5.abs())`, which is `-1.5`. Parenthesize when the receiver is meant to be
the negative number.

**`..` binds looser than arithmetic and tighter than `||`.** `0..n - 1` is
`0..(n - 1)`, which is how every range in the corpus is written.

Comparisons parse left-associatively but do not chain usefully: `1 < 2 < 3` is
`(1 < 2) < 3`, which is a `Bool` compared with an `Int` and reports twice —
`Y001` for the mismatch and `Y006` because `Bool` has no order.

Assignment is not in the table because it is not an expression. `=`, `+=`, `-=`,
`*=`, `/=` and `%=` are statements; see [Bindings](bindings.md).

## The Int helpers in the prelude

`abs`, `sign`, `min`, `max`, `clamp`, `gcd` and `lcm` are free functions, and
every one of them is **`Int`-only** — `min(1.0, 2.0)` is a type error, not a
polymorphic call. `pi()` and `e()` are the two `Float` constants. See
[The prelude](prelude.md).
