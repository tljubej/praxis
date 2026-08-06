# Appendix B: Grammar

The concrete grammar, derived from the code that implements it:
`crates/praxis-parser/src/lex.rs` for the token set,
`crates/praxis-parser/src/parse.rs` for the productions and the precedence
table, `crates/praxis-syntax/src/kind.rs` for the node kinds, and
`crates/praxis-input-parser/` for the `read` DSL. Where the design document's
Appendix A disagrees, the last section says so.

This is a reference, not a specification. The parser is recursive descent with
a Pratt loop for infix operators
([ADR-004](../../../decisions/004-parser-technique.md)), it produces a lossless
tree that retains every byte including trivia
([ADR-003](../../../decisions/003-lossless-tree-uses-rowan.md)), and it recovers
from an error rather than stopping — so a production below describes what is
*accepted*, and every rejection carries a `P0xx` or `T0xx` diagnostic rather
than a silent reinterpretation.

## Notation

```text
x?          zero or one
x*          zero or more
x+          one or more
a | b       alternatives
"..."       a literal token
UPPER       a lexical class (Ident, IntLit, …)
NEWLINE     a line break in the trivia before the next token — not a token
```

`NEWLINE` is written where the grammar consults the line break, which is only
in three places (see [Two ambiguity rules](#two-ambiguity-rules)). It is trivia
everywhere else.

## Lexical structure

### Trivia

| kind | spelling |
|---|---|
| whitespace | any run of space, tab, CR, LF |
| line comment | `//` to the end of the line |
| block comment | `/* … */`, **nestable** |

`/* outer /* inner */ still outer */` is one comment. Trivia is kept in the
syntax tree, which is what lets the editor tooling and the crash debugger point
at exact source ranges.

### Identifiers

An identifier is a Unicode identifier: one `XID_Start` scalar (or `_`) followed
by `XID_Continue` scalars. `λ`, `_x`, `snake_case` and `x_` are all names.

A **lone** `_` is not an identifier. It is its own token and it is legal only in
binding positions — `var _ = f()`, `fn g(_)`, `|_| 0`, and a wildcard pattern —
where it introduces no name. Reading `_` as a value is `P001: expected an
expression`
([ADR-049](../../../decisions/049-the-wildcard-binds-nothing-and-a-newline-ends-a-statement.md)).

### Keywords

Seventeen, and that is the whole list:

```text
var  fn  if  else  while  for  in  loop  match  return  break  continue
read  struct  enum  true  false
```

Everything else is an identifier. `out`, `panic`, `dbg`, `assert`, `Int`,
`Text`, `Vec`, `Map`, `min`, `max`, `parse`, `lines`, `int` — none of them is a
keyword. They are prelude names, type names, or names the grammar recognizes by
*position*: `parse` is syntax only when followed directly by `(`, `min`/`max`
form an assignment operator only when followed immediately by `=`, and the
parser-DSL names mean anything only inside a `read` or `parse` body.

### Literals

```text
digits    := digit ("_"? digit)*
IntLit    := digits
FloatLit  := digits "." digits exponent?
           | digits exponent
exponent  := ("e" | "E") ("+" | "-")? digits
TextLit   := '"' (char | escape)* '"'          -- no unescaped "{" in it
CharLit   := "'" (char | escape) "'"

interp    := InterpOpen expr (InterpMiddle expr)* InterpClose
InterpOpen   := '"' (char | escape)* "{"
InterpMiddle := "}" (char | escape)* "{"
InterpClose  := "}" (char | escape)* '"'
```

A `{` in a text literal opens an **interpolation hole** (§8.1), so a literal
holding one is not a `TextLit` at all: it lexes as the fragment run above, with
the hole's ordinary expression tokens between the fragments. Each fragment
carries a delimiter at both ends, so the token stream still tiles the source.
A `\{` is a literal brace and opens nothing; a `}` outside a hole closes nothing
and needs no escape. A literal that does not close on its line is one `TextLit`
plus `T004`, holes or not — the lexer splits only a literal it has already
proved closes
([ADR-147](../../../decisions/147-a-hole-renders-anything-because-the-program-wrote-the-hole.md)).

A `_` between digits belongs to the literal: `1_000`, `3.141_592` and `1e1_0`
are each one token. A trailing `_` is not — `1_` is `1` followed by the
wildcard token.

Three rules keep a `.` out of a number where it should not be:

- A `.` opens a fraction only when a **digit** follows it. So `1..5` is a range,
  `1.method()` is a method call, and `2.` is the integer `2` followed by `.`.
  There are no leading-dot floats: write `0.5`.
- A digit run immediately after a bare `.` token is a **tuple index** and takes
  no fraction. `t.0.1` is two indices, not an index and the float `0.1`.
- `1.5..2.5` is two floats and a range, because the `.` in front of the `2` was
  consumed into the `..`.

A text literal's escapes are `\"`, `\\`, `` \` ``, `\n`, `\r`, `\t`, `\0`. Any
other backslash is `T005`. A raw newline inside a text literal is not allowed.

A character literal holds **exactly one** Unicode scalar value, and takes a text
literal's escapes plus `\'` — there are no `\x` or `\u{…}` forms, because there
are none there
([ADR-141](../../../decisions/141-a-character-is-one-token-and-a-literal-is-a-load.md)).
`'é'` is one character, not two bytes. A body that names no character (`''`) or
more than one (`'ab'`) is `T007`, and an unterminated literal is `T006` naming
its own line — the same rule a template follows. Those two codes are where
`"##"[0]`'s silent truncation and `""[0]`'s run-time index fault went.

A backtick template is **one token**, interior and all. It ends at the first
backtick at brace depth zero — so a capture may hold a nested template
(`` `{g:choice(A: `{x:int}`)}` ``) and a brace inside a string
(`` `{c:one_of("{")}` ``) does not extend it. A template ends at the line it
opens on; a raw newline may not appear inside one, and an unterminated template
is `T002` naming its own line
([ADR-094](../../../decisions/094-a-template-ends-at-the-line-it-opens-on.md)).

### Operators and punctuation

Lexed by longest match, so the multi-character forms are never their prefixes:

```text
->   =>   ==   !=   <=   >=   ..   ..=   ||   &&
+=   -=   *=   /=   %=
(  )  {  }  [  ]  ,  .  :  ;  |  &  +  -  *  /  %  =  !  <  >  ?  #
```

`&&` and `||` are single tokens, so a bare `&` is never half of one. `#`, `&`
and `?` are lexed but have no production: writing one is a parse error where it
stands.

## Two ambiguity rules

Two shapes in this grammar are genuinely ambiguous, and both are settled by
**position** rather than by a new token. They are stated here because both
change how ordinary code parses.

### A newline ends a statement, and never an expression

Statements are separated by `;`, a line break, or the closing `}`/end of file —
and by nothing else. Two statements run together on one line is `P002`
([ADR-049](../../../decisions/049-the-wildcard-binds-nothing-and-a-newline-ends-a-statement.md)).

```praxis
// ADR-049 in one line: a newline ends a statement, and nothing else does
// except `;` and the closing brace. Two statements run together on one line
// have no separator, and the parser says so rather than guessing.
var a = 1 var b = 2
out(a + b)
```

```console
$ praxis check statement_separator.px --color never
error[P002]: expected `;` or a line break between statements

  statement_separator.px:4:11
  4 | var a = 1 var b = 2
    |           ^^^ expected `;` or a line break between statements

praxis: 1 error(s)
```

The line break is consulted in exactly three places, and **nowhere in the infix
operator loop**:

1. Between statements, and between the members of a `struct`/`enum` body and the
   arms of a `match` (where it is interchangeable with a comma).
2. After `break` and `return`: a value follows only if it is on the same line.
3. In front of a `(` or a `[`, and in front of the `(` of a would-be method
   call. A line-leading bracket **begins** an expression instead of continuing
   the one above it, so a tuple pattern on the line after a match arm body is
   not read as a call.

The stated cost of rule 3: a call whose callee ends one line and whose argument
list begins the next is two expressions, and so is a subscript whose receiver
and bracket are split the same way. Nothing warns about it — the third case in
the program below type-checks and runs, and `praxis check` says nothing at all —
and the fix is to move the bracket up a line.

```praxis
// A newline ends a *statement*. It never ends an expression — but a `(` or a
// `[` that begins a line starts something new rather than continuing the
// expression above it.

// One expression across three lines: the operator loop never looks at line
// breaks.
var a = 1 +
    2 +
    3
out(a)

// A method chain across lines, for the same reason: the `.` continues.
var b = [1, 2, 3]
    .map(|x| x * 2)
    .sum()
out(b)

// But a line-leading `[` opens a list literal. `d` is `c`, and the `[0]` on the
// next line is a separate expression statement — not a subscript.
var c = [1, 2, 3]
var d = c
    [0]
out(d)

// `break` takes a value only when the value is on the same line. Here it is,
// so the loop yields it.
var n = 0
var found = loop {
    n = n + 1
    if n > 2 { break n }
}
out(found)

// Here it is not: the `break` is value-less and the loop yields Unit. The line
// after it is a separate statement, and unreachable.
var m = 0
var nothing = loop {
    m = m + 1
    if m > 2 {
        break
        out(m * 100)
    }
}
out(nothing)
```

```text
6
12
[1, 2, 3]
3
Unit
```

### A record literal is legal wherever the brace cannot be a block

`if p { … }` could be a record literal `p { … }`, or the condition `p` followed
by the then-block. Four keyword heads have the problem — the conditions of `if`
and `while`, `for`'s iterator, `match`'s scrutinee — and all four resolve it by
**suppressing** a bare `Name { … }` in the head expression and in its operands.

Every bracket re-admits it. Inside `(…)`, `[…]`, an argument list, a block or a
match arm body, the grammar already knows what closes the enclosing construct,
so no `{` there can be the block a keyword is waiting for
([ADR-050](../../../decisions/050-record-literals-are-legal-wherever-a-brace-cannot-be-a-block.md)).

A closure body inherits the ambient suppression rather than resetting it: `|` is
not a bracket the grammar closes over.

```praxis
// ADR-050 in one program: a record literal is legal wherever the `{` cannot be
// a block.
//
// The four keyword heads — `if` and `while` conditions, `for`'s iterator,
// `match`'s scrutinee — claim the next `{` as their body, so a bare
// `Name { … }` is suppressed there and in every operand of the head expression.
// Every bracket re-admits it: inside `(…)`, `[…]`, an argument list, a block,
// or a match arm body, nothing else is waiting for that brace.

struct Point { x: Int, y: Int }

var origin = Point { x: 0, y: 0 }

// Suppressed at the head, allowed again inside the parentheses.
if (Point { x: 0, y: 0 }) == origin { out("same") }

// Allowed in an argument list, in a list literal, and in a match arm body.
fn shift(p, dx) { Point { x: p.x + dx, y: p.y } }
out(shift(Point { x: 1, y: 2 }, 3).x)
out([Point { x: 4, y: 5 }].len())
out(match origin.x {
    0 => Point { x: 9, y: 9 }
    _ => origin
}.x)
```

```text
same
4
1
9
```

## Files, statements and declarations

```text
source_file  := statement*

statement    := var_stmt
              | fn_item
              | struct_item
              | enum_item
              | assign_stmt
              | expr_stmt
              -- separated by ";" | NEWLINE | end of block

var_stmt     := "var" binder (":" type)? "=" expr
binder       := Ident | "_"

assign_stmt  := Ident assign_op expr                -- a bare name target
              | expr assign_op expr                 -- a place: m[k], p.x
              | expr update_op expr                 -- m[k] min= v
assign_op    := "=" | "+=" | "-=" | "*=" | "/=" | "%="
update_op    := ("min" | "max") "="                 -- adjacent, no space

expr_stmt    := expr

fn_item      := "fn" Ident ("(" param_list? ")")? ("->" type)? block
param_list   := param ("," param)* ","?
param        := binder (":" type)?

struct_item  := "struct" Ident "{" (field (member_sep field)* member_sep?)? "}"
field        := Ident ":" type

enum_item    := "enum" Ident "{" (variant (member_sep variant)* member_sep?)? "}"
variant      := Ident ("(" type ("," type)* ","? ")")?

member_sep   := "," | NEWLINE
```

Parameter and return annotations are both optional, and so is the parameter
list itself; an unannotated parameter's type is inferred from use.

A `min=` / `max=` operator is two tokens because `min` is an ordinary
identifier, so the grammar decides it by **adjacency** exactly as it does for
`+=`: the `=` must immediately follow the name with no trivia between. Written
with a space, `m[k] min = v` is not that operator, and the run-on is `P002`.

A block's value is its trailing expression:

```text
block        := "{" statement* "}"
```

## Expressions

```text
expr         := prefix (infix_op expr)*     -- folded by the precedence table

prefix       := "read" parser_expr
              | closure
              | ("-" | "!") expr
              | atom
              postfix*

postfix      := "(" arg_list? ")"           -- same line as what it follows
              | "[" arg_list "]"            -- same line
              | "." IntLit                  -- tuple element
              | "." Ident "(" arg_list? ")" -- method call, "(" on the same line
              | "." Ident                   -- field

arg_list     := expr ("," expr)* ","?

atom         := literal
              | interp                      -- "a{expr}b" (§8.1)
              | "(" ")"                     -- Unit
              | "(" expr ")"                -- grouping
              | "(" expr ("," expr)* ","? ")"  -- tuple: the first "," makes it one
              | "[" arg_list? "]"           -- list literal (a Vec)
              | block
              | if_expr | while_expr | for_expr | loop_expr
              | break_expr | continue_expr | return_expr | match_expr
              | name_or_call

literal      := IntLit | FloatLit | TextLit | CharLit | BacktickTemplate
              | "true" | "false"

-- `interp` is an atom, not a literal: its holes are expression subtrees, so it
-- has children where a literal is a leaf.

name_or_call := "parse" "(" expr "," parser_expr ")"
              | Ident type_arg_list? "(" arg_list? ")"
              | Ident "{" record_field_list? "}"   -- record literal, if allowed
              | Ident

type_arg_list    := "[" type ("," type)* ","? "]"
record_field_list:= record_field ("," record_field)* ","?
record_field     := Ident (":" expr)?        -- `{ x }` puns, `{ x: e }` is explicit

closure      := "|" (cparam ("," cparam)* ","?)? "|" expr
              | "||" expr                    -- the zero-parameter form
cparam       := pattern (":" type)?

if_expr      := "if" expr_no_record block ("else" (if_expr | block))?
while_expr   := "while" expr_no_record block
for_expr     := "for" pattern "in" expr_no_record block
loop_expr    := "loop" block
break_expr   := "break" expr?                -- value only on the same line
continue_expr:= "continue"
return_expr  := "return" expr?               -- value only on the same line
match_expr   := "match" expr_no_record "{" (arm (arm_sep arm)* arm_sep?)? "}"
arm          := pattern "=>" expr
arm_sep      := "," | NEWLINE
```

`expr_no_record` is `expr` with the record-literal suppression of
[the second ambiguity rule](#a-record-literal-is-legal-wherever-the-brace-cannot-be-a-block).

A `(` with no comma in it is a grouping; the first comma makes it a tuple, and a
trailing comma closes the list. **A tuple has two elements or more**, so `(1,)`
is refused at the comma — `P001: a tuple has two elements or more, so this comma
names nothing` — and the node recovers as the grouping `(1)`. The same rule
holds in type position: `(Int,)` is refused where `(Int, Text,)` is fine.

It used to parse as a tuple node holding one element, which put two passes in
disagreement about the same node: inference collapsed the arity-one tuple back
to `Int` while lowering, reading the node kind, built a tuple object. MIR
verification caught it, as an abort with no source span, three passes past the
comma. An empty `()` parses and evaluates to `Unit`. A `[` always builds a list, at every
arity including the empty `[]`, whose element type comes from its use.

`type_arg_list` is legal after exactly eleven names — `Vec`, `Deque`, `Map`,
`Set`, `Counter`, `MinHeap`, `MaxHeap`, `BitSet`, `Grid`, `Range`, `Option` —
and only immediately before a `(`. Nothing else can tell `Counter[(Int, Int)]()`
from `m[key]`: the brackets are the same two characters and `(Int, Int)` is a
legal tuple expression, so the *name* breaks the tie. The stated cost is that a
binding shadowing one of those names cannot be subscripted.

`read` takes a **parser expression**, not an ordinary one, so its operand ends
where the parser grammar ends: `read int + 1` is `(read int) + 1`.

### Operator precedence

The Pratt table, loosest first. Every infix operator is left-associative.

| binding power | operators | notes |
|---|---|---|
| 1 | `\|\|` | logical or |
| 3 | `..` `..=` | a range is its own node, not a binary operator |
| 5 | `&&` | logical and |
| 7 | `==` `!=` `<` `>` `<=` `>=` | parsed left-associative |
| 9 | `+` `-` | |
| 11 | `*` `/` `%` | |
| 13 | prefix `-` `!` | binds tighter than every infix operator |

Consequences worth knowing:

- `a || b && c` is `a || (b && c)`, and `a == b && c == d` is
  `(a == b) && (c == d)`.
- `0..n - 1` is `0..(n - 1)`. A range bound is an arithmetic expression, which
  is how every range in the corpus is written
  ([ADR-059](../../../decisions/059-a-range-is-a-value-and-a-descending-one-is-empty.md)).
- Comparison binds **tighter** than `..`, so `0..3 == 0..3` parses as
  `(0..(3 == 0))..3` and is a type error rather than a range comparison.
- `-a * b` is `(-a) * b`, and `!p && q` is `(!p) && q`.

Where `&&` sits relative to `..` is arbitrary — a range of `Bool`s and a range
bound that is a `&&` are both nonsense — and it was placed where it moves the
fewest numbers.

Assignment is **not** an infix operator. `=` and `+=` are statement-level, so
`a = b = c` does not parse as an expression.

## Patterns

```text
pattern       := "_"
               | IntLit | TextLit | CharLit | "true" | "false"
               | Ident                                       -- bind, or a payload-less variant
               | Ident "(" pattern ("," pattern)* ","? ")"   -- variant with payload
               | Ident "{" pattern_field_list "}"            -- record
               | "{" pattern_field_list "}"                  -- headless record
               | "(" pattern ("," pattern)* ","? ")"         -- tuple

pattern_field_list := pattern_field ("," pattern_field)* ","?
pattern_field      := Ident (":" pattern)?     -- `{ x }` puns, `{ x: p }` is explicit
```

A pattern's `{` is never ambiguous the way a record *literal*'s is: a pattern is
followed by `=>`, by `in`, or by the `|` that closes a closure's parameters —
never by a block. That is also what makes the head optional — a leading `{` in
pattern position can only open fields, so `for {x, y} in points` and
`|{x, y}: Point| x + y` need no new token
([ADR-091](../../../decisions/091-a-variant-patterns-enum-is-the-scrutinees.md)).
A headless record pattern still has to learn *which* record it matches from
somewhere: the `for` gets it from the iterator, and the closure needs the
annotation shown, or it is `Y123: { … } cannot tell which record it matches
here`.

Parentheses in pattern position are **always** a tuple. There is no grouping
form, because a pattern has no precedence to override, so `(p)` is a
one-element tuple pattern — which is `Y123: a tuple pattern names two elements
or more`, whatever the scrutinee is. `()` and a headless `{}` are rejected at
the parser: they bind nothing and test nothing, and the pattern that matches
anything is spelled `_`.

Patterns appear in three binding positions and they are one grammar in all
three: match arms, the `for` binding, and closure parameters.

## Types

```text
type      := atom_type ("->" type)?                -- function type, right-associative
atom_type := Ident ("[" type ("," type)* ","? "]")?
           | "(" (type ("," type)* ","?)? ")"      -- tuple (2+) or grouped type
```

`A -> B -> C` is `A -> (B -> C)`. A parenthesized single type is that type; two
or more is a tuple. An unknown identifier parses fine here and is rejected by
name resolution — so a typo in a type annotation is `N002: unknown type` and not
a syntax error. Unlike `N001`, which suggests the nearest name in scope, `N002`
carries no suggestion.

## The input-parser grammar

`read` and `parse` are the two doors into a second grammar, implemented in
`crates/praxis-input-parser/`. Whitespace and comments between its tokens are
insignificant; whitespace *inside* a backtick template is significant and has
its own rules.

```text
read_expr    := "read" parser_expr
parse_expr   := "parse" "(" expr "," parser_expr ")"

parser_expr  := atomic | template | call

atomic       := Ident                          -- one of the ten names below
call         := Ident "(" (arg ("," arg)* ","?)? ")"

arg          := parser_expr                    -- positional parser, or a bare flag
              | TextLit                        -- a string literal
              | Ident ":" parser_expr          -- a named argument
              | Ident ":" literal              -- a keyword argument's value
```

### Atomic parsers

A closed set of ten:

| name | matches | yields |
|---|---|---|
| `int` | signed decimal integer | `Int` |
| `uint` | non-negative decimal integer | `Int` |
| `float` | decimal floating-point number | `Float` |
| `byte` | a decimal integer in `0..=255` | `Byte` |
| `char` | one Unicode scalar | `Char` |
| `digit` | one decimal digit | `Int` |
| `word` | a run up to the next space, tab, comma or line break | `Text` |
| `identifier` | an identifier run (the language's own rule) | `Text` |
| `text` | the whole region, which the literal after it bounds | `Text` |
| `rest` | the whole region | `Text` |

`text` and `rest` are one rule — take the region — and differ only in what
usually surrounds them. The bound is the **first** occurrence of the literal
that follows, and there is no backtracking: `` `{a:text}-{b:word}` `` reads
`x-y-5` as `a = x`, `b = y-5`, and `` `{a:text}-{b:int}` `` on the same line
faults rather than trying the second `-`.

A name that is not on this list and is not followed by `(` is
`I010: unknown atomic parser`, reported at the name — not a silent fallback to
`int`.

### Templates

```text
template      := "`" template_part* "`"
template_part := literal_run | ws_escape | capture
capture       := "{" Ident ":" parser_expr "}"    -- named
               | "{" parser_expr "}"              -- anonymous
```

A capture body is a **full parser expression**, including a constructor call and
a nested template: `{items:csv(int)}` and `` {g:choice(A: `{x:int}`)} `` both
parse. The closing `}` is found brace-, paren-, string- and template-aware, so
`{c:one_of("}")}` and `{xs:sep(",", int)}` close where they should. Nesting —
of templates, of `{`, and of `(` — is bounded at 32 levels, and exceeding it is
a diagnostic rather than a stack overflow.

Whitespace inside a template:

| written | matches |
|---|---|
| a run of spaces or tabs | one or more spaces or tabs (flexible, for column alignment) |
| `\s*` | zero or more spaces or tabs |
| `\s+` | one or more spaces or tabs |
| `\x20` | exactly one ASCII space |
| `\n` | one line ending |
| `\t` | one tab |

A space run at **either** end of a literal is flexible whitespace and not part
of the text it borders.

The only other escapes inside a template are `` \` `` and `\\`, which stand for
those characters. There is no escape for `{` or `}` — `\{` is `I030: invalid
escape`.

Named and anonymous captures may not be mixed in one template — that is `I020`.
All-named produces a record, one anonymous capture produces the scalar, and
several anonymous captures produce a tuple.

### Constructors and their argument shapes

Thirteen constructors, with the shape each one's argument list must have, plus
the `repeated(P)` marker below the table:

| constructor | shape |
|---|---|
| `lines(P)` | one parser |
| `sections(P)` | one parser (homogeneous) |
| `sections(name: P, …)` | named arguments only (heterogeneous); any may be `name: repeated(P, N)`, and the last may be `name: repeated(P)` |
| `csv(P)` | one parser |
| `ws(P)` | one parser |
| `sep("s", P)` | a string literal, then a parser |
| `grid(P)` | one parser |
| `grid(P, ragged, fill: v)` | `ragged` and `fill:` come together or not at all |
| `matrix(P)` | one parser |
| `chars(P)` / `chars(P, skip: policy)` | one parser and an optional `skip:` |
| `one_of("set")` | one string literal |
| `block(item, …)` | one or more parsers and/or `name: P` items |
| `choice(Name: P, …)` | named arguments only, at least one |
| `optional(P)` | one parser |
| `scan(P)` | one parser |
| `repeated(P)` | a `sections` named argument, and only its last: it takes every section left |
| `repeated(P, N)` | any `sections` named argument: it takes exactly N sections |

`skip:` takes `none`, `whitespace` or `newlines`. `fill:` takes a non-empty
literal, which is the text a short row is padded with — it is not checked
against the cell parser, so `grid(char, ragged, fill: 0)` pads with the
character `0`. `repeated(...)` is not a parser in its own right: outside a named
argument of a `sections` call it is `I028`. The uncounted `repeated(P)` is
greedy, so it is also `I028` anywhere but last; `repeated(P, N)` is bounded and
may be followed. `N` is a whole-number literal of at least 1 — the parser plan is
built when the program is compiled, so a count read from a value cannot exist.

A constructor's argument-list *shape* is checked before anything is built, so a
wrong argument is reported rather than dropped.

## Where the design document's Appendix A drifted

Appendix A of `praxis_technical_design.md` sketches this grammar. It is
labelled "illustrative EBNF, not the final parser source", and four things in
it are no longer true of the implementation:

- **`argument := parser_expr | IDENT ":" parser_expr`** misses three forms. A
  string literal is a positional argument in its own right (`sep(",", int)`,
  `one_of("LR")`); a positional whole number is one too (`repeated(lines(int), 6)`);
  and a named argument's value may be a **literal** rather than a parser
  expression (`fill: 0`, `fill: "-"`). All three are productions the sketch has
  no rule for.
- **`template_part := template_literal | capture`** misses the whitespace-policy
  escapes. `\s*`, `\s+`, `\n`, `\t` and `\x20` each scan into a part of their
  own, which is what makes a leading or trailing space run in a literal mean
  "flexible whitespace" rather than "these exact bytes". Section 7.2 of the same
  document lists them; Appendix A's grammar does not.
- **`parser_root := parser_expr EOF`** has no counterpart in the compiler. There
  is no standalone parser-expression entry point; `read` and `parse` are the two
  and only ways in.
- **`atom := IDENT`** is where a bare flag (`ragged`) and a keyword value
  (`whitespace`, `newlines`, `none`) end up syntactically, but they are not
  atomic parsers. Which of the three an identifier is depends on the
  constructor it appears in, and that is decided after parsing, by the shape
  table above.

The sketch is otherwise accurate, including the part that reads most like an
aspiration: a capture body really is a full `parser_expr`.

Appendix A's expression grammar has one drift of its own, in the other
direction: it has no production for an interpolated text literal, because §8.1's
interpolation was specified in prose and unimplemented when the sketch was
written. It is implemented now, and the `interp` production above is the shape
of it. Note that an interpolated literal is deliberately **not** a `pattern`:
a pattern tests a constant, and `match s { "{x}" => … }` is reported rather
than read as a binding.
