# Milestone 12 handover — LSP completeness, and the formatter that is not in it

**Date:** 2026-08-05
**Status:** M12 complete except the formatter, which is **out of scope by the
maintainer's direction** (§4 below). `just ci` green.
**Predecessor:** `20-milestone-11-handover.md` (the language server MVP)

> For a fresh context, read in this order: ADR-130, ADR-131 and ADR-132 — they
> are the shape of the milestone — then `crates/praxis-lsp/src/lib.rs`'s module
> map, then `crates/praxis-lsp/tests/m12.rs`, which is §19.12's deliverables and
> acceptance criteria written as assertions.

## 1. What shipped

| §19.12 deliverable | Where | Gate |
|---|---|---|
| Find references | `praxis-lsp::navigation` | the **shadowed** binding's uses are its own, and not the binding it shadows |
| Rename | `praxis-lsp::rename` (ADR-131) | three shapes of collision refused; the same rename to a free name accepted |
| Workspace symbols | `praxis-lsp::workspace` | a folder walk, `target/` skipped, and the **open buffer beating the file on disk** |
| Inlay hints | `praxis-lsp::inlay` | `fn foo(a, b)` reads as `fn foo(a: Int, b: Int)`; `?T` shown; applying an accept-edit leaves the file clean |
| Code actions | `praxis-lsp::code_action` (ADR-132) | four families, each applying the offered edit and re-analyzing |
| Method and parser documentation in hover | `praxis-lsp::hover`, `Constructor::doc`, `AtomicKind::doc` | the catalog's own sentence and the constructor table's own sentence |
| Stable formatter | **not done — see §4** | the capability is not advertised, and a test asserts its absence |

One prerequisite was not on the list and had to be built first: **exhaustiveness
diagnostics did not reach the editor at all**, so "add missing match arms" had
nothing to trigger on. §2.1.

Three ADRs landed with the code that implements them: **130** (coverage is
analysis's answer, and the pattern shape is built once), **131** (a rename is
safe when re-resolution is unchanged), **132** (a code action is a diagnostic's
machine-applicable suggestion).

## 2. The five things worth not rediscovering

### 2.1 `praxis check` was silent on a non-exhaustive match, and so was the editor

`exhaustive::check` had exactly one caller: `Lowerer::lower_match`. Neither
`praxis check` nor the language server lowers, so a program with a missing arm
**checked clean and failed to run** — REP-12's asymmetry, for every `match` in
the language, still open after several milestones of removing it elsewhere. §15.2
lists exhaustiveness errors among the diagnostics the server must publish; it
never could.

The fix is ADR-130 and it is a small extraction with a large blast radius:
pattern shape moved out of the lowerer into `praxis_hir::pattern`, and
`exhaustive::check_matches` runs over every `MATCH_EXPR` at the end of `analyze`.
Both consumers now read one builder, which matters more than it sounds: the one
question `Y120` turns on is whether a bare `Name` is a variable binding or a
payload-less variant, and two builders would have been free to disagree about it.

**This is a behaviour change for existing programs.** A file that checked clean
and failed to run now fails to check, at the same span with the same message.

### 2.2 Coverage cannot be checked while inference is on the stack

The first implementation called the checker from `infer_match`. It reported
`Y120` on programs that are fine:

```praxis
fn f(e) -> Int { match e { A => 1  B => 2 } }
out(f(A))
```

At `infer_match` the scrutinee is still a type variable — the call on the last
line is what pins it — and a variable has no signature to enumerate, so the
checker asks for a `_` arm the program does not need. **A false positive is worse
than the silence it replaced.** The pass runs after inference for that reason,
and `a_scrutinee_pinned_later_in_the_file_is_not_reported` is the gate that says
so.

### 2.3 A "did you mean" needs one threshold, not four

Four passes had the same shape of mistake to report — an unknown constructor, an
unknown atomic, an unknown name, an unknown method — and each could have picked
its own idea of "close enough". `praxis_source::suggest::nearest` is the one
rule: edit distance within `max(1, len / 3)`, counted in **characters** so a
non-ASCII identifier (§4.1 permits them) is not silently excluded.

The threshold is load-bearing in both directions, and the tests pin both: `line`
is offered `lines`, and `abc` is offered nothing at all for `xyz`. It also
refuses more than a reader expects — `lenght` is three edits from `len`, so
`v.lenght()` gets no fix. That is the rule working: at a budget wide enough to
catch it, `abc` starts suggesting `xyz`.

### 2.4 A closure parameter's name is not a direct child of its `PARAM`

`fn f(a: Int)` parses as `PARAM [ Ident, COLON, TYPE_REF ]`, and `|a: Int|`
parses as `PARAM [ PATTERN [ Ident ], COLON, TYPE_REF ]` — the closure's
parameter list goes through `parse_pattern`. An inlay-hint implementation that
looks for the declared name among a `PARAM`'s **direct child tokens** therefore
finds it for a `fn` and misses it for a closure, and the symptom is not a missing
hint: it is a hint offered *next to the annotation the author already wrote*,
because the "does this already have a type?" test never ran.

`declared_name` searches descendants and skips anything inside a type node.
`an_annotated_binding_gets_no_hint` covers all three forms in one fixture.

### 2.5 A rename check is a re-analysis, not a list of collision kinds

ADR-131 has the argument in full. The short version: the four ways a rename can
change meaning are not a list anybody can be sure they finished, and the scope
tree cannot answer "which scope is this offset in" anyway (M11 handover §5.2).
Applying the edit to a copy and requiring name resolution to come out the same
answers the question directly, costs one analysis (~4 ms), and catches capture in
both directions — including the one nobody writes a case for: a reference to
*another* binding of the new name that starts resolving to the renamed one.

## 3. What the inlay hints show, and why `?T` is one of them

The maintainer's request was rust-analyzer's behaviour: `fn foo(a, b)` reading as
`fn foo(a: Int, b: Int)` in the editor, with the `?T` placeholder shown where
inference has not pinned a type. Both hold. The rule is one line — **every
binding whose type the source does not already state** — read off
`Analysis::decls`, which is where a `fn` parameter, a closure parameter, a `var`,
a `for` variable and a pattern binding all already are (they are one thing;
ADR-125).

`?T` is `db.render`'s own spelling for an unbound variable, the same one hover
and `praxis check` print. Hiding it would make "no hint" mean two different
things — *the source states this type* and *the compiler does not know it* —
which are the two cases a reader most needs told apart.

§15.2 suggests keeping hints "off by default or conservative". They are **on**,
and conservative in what they hint rather than in whether they appear: never
beside an annotation, and a parser root is hinted only where the binding does not
already name the same type. A user who wants them off has the editor's own
`editor.inlayHints.enabled`; the server has no setting of its own, because a
setting is a second place for the answer to live.

Each hint carries a `TextEdit` that writes the annotation into the file — but
only where the annotation is both legal (a `PARAM` or a `VAR_STMT`; a `for`
variable has no annotation syntax) and spellable (`?T` is not, and neither is an
anonymous record). `applying_a_hints_edit_keeps_the_file_clean` is what makes
that claim testable rather than asserted.

## 4. The formatter is deliberately not here

§19.12 lists a stable formatter and §15.2 states its rules. **It is out of scope
for this milestone at the maintainer's explicit direction**, taken as a scope
decision rather than as an omission to fix later without asking.

What follows from that, and is not left implicit:

- `documentFormattingProvider` is **not advertised**, and neither are the range
  and on-type variants. The handshake test asserts their absence. Advertising a
  formatter the server does not have would make an editor stop offering its own
  behaviour and then do nothing on `Format Document` — worse than the feature
  being visibly missing.
- §19.12's second acceptance criterion — *"Formatter preserves template
  semantics byte-for-byte except documented escape normalization"* — is
  therefore **unmet and unclaimed**. The skeleton in `praxis-parser` (ADR-005) is
  untouched.

## 5. What is still not here

**Multi-file analysis.** Rename, references and diagnostics are one file.
`workspace/symbol` reads the folder, and it is the only query that does — it
parses, it does not analyze, so it needs no cross-file types. A rename that
crosses files needs a workspace-wide analysis, and ADR-131's rule extends to it
unchanged (the same comparison over the set of files).

**A persistent workspace index.** The symbol walk is per query, bounded by
`MAX_FILES`/`MAX_DEPTH`, with `target/`, `node_modules/` and dotted directories
skipped. An AoC workspace is tens of small files and parsing one is under a
millisecond; a cache would need file-system events the server would then have to
be right about. If a measurement ever asks for one, the walk is the thing to
replace and `Walk::truncated` already says when it stopped early.

**Semantic token deltas**, and **incremental reparse**. Both were named as out of
scope at M11 and remain so.

**Unused-binding warnings.** §15.2's diagnostics list ends with *"unused binding
warnings, configurable"*, and no such warning exists — in the language server or
in `praxis check`. It is on **M11's** deliverable list rather than §19.12's, so
this milestone did not take it, but it is the one line of §15.2 that is still
unwritten and it is recorded here rather than left to be noticed. Two things it
would need that nothing else in the server has yet: a warning code in ADR-051's
register, and a configuration channel (`workspace/didChangeConfiguration`), since
"configurable" is the requirement's own word.

**`praxis watch` and `praxis repl`** are still unimplemented, and the extension's
`Praxis: Watch File` command still says so in its own title.

## 6. Numbers

Not re-measured; §15.5's targets were measured at M11 and nothing in this
milestone changes the analysis path except the coverage pass, which builds
patterns (not bodies) for the matches in a file. The two queries with a cost
worth naming:

- **Rename** runs a second full analysis by design (ADR-131) — one file, ~4 ms in
  a debug build.
- **`workspace/symbol`** parses every `.px` file under the roots per query. It
  does not analyze them, and the walk is capped.

## 7. Register

No open rows. Two decisions are recorded rather than taken:

- **The formatter** (§4), which is the maintainer's scope call.
- **M11's §5.3 residual** — whether the server suppresses diagnostics on the
  statement containing the cursor while it is syntactically incomplete — is
  untouched and still registered rather than decided.

The two standing decisions from handover 18 are also untouched: **REP-67** (the
`praxis_alloc_text` split) and **D19** (whether there is a character literal).
