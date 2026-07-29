# ADR-056: `panic`, `assert` and `dbg` are real functions with real types and a real fault

**Date:** 2026-07-29
**Status:** Accepted — implemented
**Milestone:** Repair (stage S17 — TY-33, unit 1 of 4)

## Context

§16.1 lists fifteen prelude names that are strings and nothing else. A
reference to one gets a fresh type variable, so it unifies with anything, and
the call then lowers as `CallTarget::User(name)` — a direct call to a function
nobody defined. That is TY-33, and D5 answers it: **implement all fifteen**,
with `panic` first.

`panic` first for the reason D5 gives. It is the only one of the fifteen that
**typechecks today and then fails the compile**:

```text
$ praxis run stop.px
error: JIT compilation failed: Cranelift error: unresolved user function `panic`
```

A clean program that cannot run is worse than a rejected one, and §9.1 lists
"explicit `panic`" as a runtime fault the language *has*. `assert` is the same
hole with a second symptom — `assert(1)` was accepted, because a fresh variable
rejects nothing. `dbg` is §8.1's debugging affordance and had no dispatch
either.

This ADR covers the first of D5's four units. The eight numeric helpers wait on
TY-31's numeric constraint, and the six graph helpers are their own unit with
their own owed decisions.

## Decision 1: each name gets the type its contract needs, and `assert`'s is monomorphic

| Name | Scheme | Why |
|---|---|---|
| `panic` | `forall T. (T) -> Never` | Unchanged; it was already seeded. `Never` is what lets `fn f() -> Int { panic("x") }` compile (TY-19, S14). |
| `dbg` | `forall T. (T) -> T` | §8.1: prints and returns the value. The identity on types is what lets it wrap any subexpression without changing what the program computes. |
| `assert` | `(Bool) -> Unit` | Monomorphic. A condition is a `Bool`, and saying so is the whole of `prelude_assert_requires_bool`. |

`panic` accepts any `T`, as `out` does, rather than requiring `Text`: the value
is rendered through its descriptor, so `panic(candidate)` says what the
candidate *was*. §9.1 asks for "an explicit message", not for a string literal.

`assert` takes **one** argument. A `assert(cond, message)` overload has no
spelling: a name has one scheme, and the type system has no arity-based
overloading. Adding a second name (`assert_with`) or an optional-argument form
is a language decision, and no finding asks for one.

## Decision 2: `panic` and `assert` are faults with their own kinds

`FaultKind` gains `Panic = 12` and `AssertFailed = 13` — two of §9.1's listed
fault kinds that had no encoding. They join the existing eleven; a fault kind
that had to borrow another's name would be the RT-17 mistake again (two callers
raising `None` for want of a kind that described them).

The alternative was to reuse an existing kind, or to have `praxis_panic` write
to stderr and abort. Neither survives the crash debugger: §9.3's snapshot,
backtrace and locals all hang off the fault path, and a `panic` that bypasses
it is a `panic` you cannot debug — which is the opposite of what §9.1 promises.
As a fault it gets all of it for free, and it does: the backtrace and the
frame's locals render exactly as a division by zero's do.

## Decision 3: the message is rendered at the raise, not at the report

`RuntimeContext` gains `fault_message`, a host-managed pointer to a
`FaultMessage` slot the `Runtime` owns — the same shape as `parse_detail` and
`crash_snapshot`, appended at the end of the struct so every
generated-code-read offset is unchanged.

The slot holds a `String`, not the argument's `GcRef`. It has to: the host
reads the message *after* it tears the heap down, so a stored reference would
outlive what it names. Rendering in the wrapper — through the value's
descriptor, exactly as `out` renders its argument — is what makes the message
outlive the value.

`assert` writes **no** message. It has no words to write: the condition is the
only thing it was given, and "assertion failed: assertion failed" is what a
default produces. The fault kind is the whole report; `panic` is the name that
carries words.

## Decision 4: the three lower to runtime symbols, in one place

`praxis_dbg`, `praxis_panic` and `praxis_assert` are three new rows in the F4
manifest and three arms in `praxis_runtime::abi::address`. All three share one
shape — one `GcRef` in, one `GcRef` out — so MIR's `build.rs` recognizes them
with one `control_builtin_symbol` lookup rather than three branches, and the
fault check after the call is driven by the manifest's own `Effect`
(`sym.faults()`) rather than by a hand-written list.

`out` keeps its own path: it returns the Unit sentinel rather than its
argument, so it does not share the shape.

## Consequences

- **`RUNTIME_ABI_VERSION` is 13.** `FaultKind` gained two variants and
  `RuntimeContext` gained a field. This is S17's one bump (H17); the stage's
  later units must not spend another.
- **`render_noninteractive` takes the message.** Seven parameters now. The
  message is appended to the fault line, because for `Panic` the kind alone
  ("panic") says nothing the program did not already say.
- **A `panic` in a `match` arm now compiles**, which is what §5.4's
  `Wall => panic("wall has no traversal cost")` example always assumed.
- **`dbg` prints through the same formatter `out` does**, to stderr. It is
  `Pure` in the manifest: it allocates nothing and cannot fault, so it is not a
  safepoint.
- **Twelve prelude names are still phantom.** `abs`, `sign`, `min`, `max`,
  `clamp`, `gcd`, `lcm` are D5's unit 2 (after TY-31); the six graph helpers are
  unit 3. `pi` and `e` already had schemes and dispatch.
