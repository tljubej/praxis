# ADR-125: A binding is a binding, and the compiler decides its storage

**Date:** 2026-08-04
**Status:** Accepted
**Milestone:** —

## Context

§4.2 gave the language two binding keywords. `let` created a binding that could
not be reassigned; `var` created one that could. Everything else about them was
identical — both stored a `GcRef`, both had a static type, both shadowed, and
both could point at an object the program went on to mutate.

The distinction was not free. It reached four places:

1. **`Y009 AssignToImmutable`**, reported for a write to anything that was not a
   `var`.
2. **Generalization** (§5.3): a `let` could be generalized under the value
   restriction; a `var` never was.
3. **Capture representation** (§4.10): a captured `var` was boxed into a
   `VarCell` and read through two runtime calls; a captured `let` was copied into
   the closure's environment.
4. **The debugger's purity check**, which refused a `var` declaration inside a
   `p` expression on the grounds that the keyword announced an intent to mutate.

And it was leaky in a way that is worth stating plainly, because it is what
decided this. `Y009` did not fire only on `let`. A **function parameter**, a
**`for` variable** and a **name introduced by a pattern** were all reported by
it too — none of them written with either keyword, and none of them with any way
for the programmer to opt out. So the language did not have "an immutable form
and a mutable form". It had a mutable form, an immutable form, and three
immutable forms with no mutable counterpart at all.

## Decision

**`let` is removed. `var` is the language's one binding form, and every binding
is assignable.** `Y009` is retired.

A parameter, a `for` variable and a pattern name are bindings in exactly the
sense a `var` is, and may be written:

```praxis
fn clamp_low(n) {
    if n < 0 { n = 0 }
    n
}
```

Rust-style shadowing is unchanged and is now the only way to rebind a name at a
new type:

```praxis
var x = 5
var x = "asdasd"    // a new binding, a new symbol id, an unrelated type
```

`let` is not reserved. The lexer does not know the word, so it is an ordinary
identifier; a file that still says `let x = 5` gets `N001` on `let` rather than a
migration message. Nothing outside this repository writes Praxis, and a reserved
word that exists only to apologize for itself is a worse artifact than a name
users can have back.

### What replaced the keyword

The four consequences above were consequences of *mutability*, and three of them
still need an answer. They now read one fact — **`Symbol::reassigned`**, set by
name resolution when it resolves the target of a `name = …` statement:

- **Generalization** is gated on it. A binding nothing writes is generalized
  under the value restriction, exactly as a `let` was; a binding something writes
  is not.
- **Capture representation** is gated on it. A captured binding gets a `VarCell`
  only if something writes it; otherwise it is copied.
- **Purity** no longer asks. A binding *declaration* mutates nothing — the write
  is `TypedStmt::Assign`, which the check already rejected on its own account.

Name resolution is the only pass that can answer this. Under shadowing the
target of a write is decided by scope and not by spelling — in
`var a = 1; a = 2; var a = "s"` the assignment writes the *first* `a` — and the
resolver is the walk that has the scope at that point.

The **generalization** gate is a soundness requirement, not a tidiness one.
Assignment instantiates the target's scheme and unifies the copy, so a
generalized binding is not constrained by being written. `var f = |x| x` is a
syntactic value and would generalize to `forall T. T -> T`; `f = |n| n + 1`
would leave it there; and `f("s")` would then type-check and call the `Int`
closure with a `Text`. That is a wrong-type call reaching the backend, not a
missing diagnostic.

## Consequences

**The set of programs the compiler accepts grew, and no program changed
meaning.** Every binding that was legal before is legal now with `let` spelled
`var`, at the same type, with the same representation: what used to be a `let` is
a binding nothing reassigns, which is precisely the set the two gates above
select. The `.px` corpus was migrated by spelling and its outputs did not move.

**Two representations improved.** A `var` nothing reassigns no longer pays for a
cell or for `praxis_var_cell_get`/`_set` on every access, and it generalizes. The
old rule over-approximated in that direction; the new one is exact.

**Five binding sites can now need a cell, where one could before.** A parameter,
a closure parameter, a `for` variable, a match arm's `Bind` and a destructuring
sub-pattern are all writable and all capturable. The MIR builder boxes at each,
through one `bind_cell` helper, and the boxing is **per binding event** rather
than per symbol: a `for` variable is a fresh binding each step, so a closure made
on step *i* keeps step *i*'s cell.

**A match arm's binding is no longer always an alias.** It used to bind the
scrutinee's local outright — free, and correct while nothing could write through
it. For `match v { n => … }` that local *is* `v`'s, so an arm that writes `n` now
gets a slot of its own. Only an arm that writes pays for it; `Bindings::reassigned`
is what the builder asks.

**`Y009` is spent and stays spent.** ADR-051's rule is that a code is a permanent
user-facing identifier. `Y009` is retired rather than reissued, and `DiagCode`
carries a comment at the hole saying so.

### The measurement

**No benchmark moved.** `ab-NOLET.json`: arm A is `86ad8d0` built from a detached
worktree, arm B is this tree, five palindromic reps each. The geometric mean over
the seven closure-free benchmarks is **0.999×** and not one delta clears the 2%
floor this machine sets — `bfs` −1.1% ±2.8%, `collatz` −0.2% ±0.4%, `hashwork`
−0.0% ±1.4%, `mandelbrot` +0.4% ±0.6%, `primes` −0.1% ±0.2%, `tree` +0.2% ±1.0%,
`vm` +0.1% ±0.7%. Every arm's stdout matched the other's byte for byte and
matched `results.json`'s recorded checksum, so the sweep is `"ok, with caveats"`
rather than void. The caveat is the load ceiling: it was raised to 8.0 against an
observed ~2 (a desktop in use, no competing build), which widens the bars the
palindrome cannot absorb.

**`pipeline` was measured apart from the other seven, and had to be.** It is the
only benchmark with a capture, and one source file cannot serve both arms: under
the old compiler `var salt` is boxed and `let salt` is copied, so feeding the
migrated source to the baseline binary would time a program the baseline tree
never contained. Run like for like — each arm on its own tree's source, both
compiling `salt` by value — it is **1.0007× ±0.0111**, nothing. Run the way the
shared sweep would have, charging the boxing to the baseline, it reads **1.0115×
±0.0061**: the `VarCell` round trip per element is real but under this machine's
floor at this workload. The gap between those two numbers is the whole reason the
benchmark was split out, and it is roughly the size of the effect being looked
for — which is what makes sharing a source across arms the wrong default for any
change that moves what a spelling compiles to.

## Alternatives considered

**Keep `let` as a reserved word with a migration diagnostic.** Cheap, and better
for a language with users. This one has none, and the word is then permanently
unavailable as an identifier in exchange for a message nobody will read twice.

**Remove only the keyword, and keep parameters / `for` variables / pattern names
immutable.** This is the smaller change and it is the incoherent one: it keeps
the immutable binding class and removes the only syntax that ever opted out of
it. The asymmetry described in the context is the argument against the split, so
a fix that preserves the asymmetry answers nothing.

**Keep the split and fix its leak — let `var` apply to parameters and `for`
variables.** A real option, and the one Rust takes with `mut`. It buys a reader
the promise that a name does not change under them, and it costs a keyword on
every binding that does change. For a language whose stated purpose is
Advent-of-Code-shaped puzzle solving (§1), where a solution is read once by the
person who wrote it ten minutes ago, the promise is not worth the annotation —
and the compiler can derive the two things the annotation was actually load-bearing
for.
