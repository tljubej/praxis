# What is left — the repair is done, the register is not

**Date:** 2026-07-31
**Tree:** `1bd85d8` (everything below was measured there) · **0 ignored tests** · `just ci` green

> **Superseded by `18-every-row-closed-handover.md` (2026-08-01).** Every row in
> both tables below is closed and both decisions are answered. **Read this for
> how the work was scoped; read 18 for what the answers were** — including the
> three places where a row below turned out to describe its own defect wrongly
> (REP-56's "a feature, not a repair", REP-33's five missing names, and REP-54's
> "a tuple descriptor from a static path with no tuple constructor"). The tables
> are left as written: they are the record of what was believed on 2026-07-31,
> and editing them would erase the thing 18 is about.

This is the successor to `16-repair-s18-s21-and-the-second-register-handover.md`,
which is still worth reading for *how* the four stages landed and for its five
"worth not rediscovering" notes. **Read 16 for the history; read this for the
work.** Handover 16 was measured at `b557e0a` and four commits landed after it,
one of which changes what its REP-56 row says.

The suite's pass count is deliberately not here. It lives in
`implementation-repair-progress.md` §1 and nowhere else; the tree above is what
makes the measurement re-runnable.

## The one-paragraph answer

**The repair proper is finished.** Every stage the plan schedules is closed
(S1…S21, plus S23…S26 which the repair added; S22 is struck as "no action"), the
acceptance gate the audit shipped is at **zero ignored tests** against a baseline
of 149, the corpus is 28 of 28, and **nothing left in the register can produce a
wrong answer silently.** REP-56 was the last one that could, and since `af9df6c`
it aborts with a defined message instead of reading past the object.

What remains is one small correctness cluster and a set of **language questions
wearing defect numbers**. The second set is larger than the first, and that is
the honest shape of it: those rows are not waiting on effort, they are waiting on
someone deciding what the language should do.

## What landed after handover 16

| Commit | What |
|---|---|
| `9dd3155` | ADR-085 (`Text + Text` is concatenation) and REP-61/62/63 — three diagnostics that pointed at the wrong thing. Landed by a **concurrent session**; see "Two sessions, one tree" below |
| `af9df6c` | **REP-56's family finished.** `044d0a4` had bounded `int_payload` alone; a reviewer found its three siblings still reading unchecked in release |
| `58c7dc1` | **REP-64** — a `Float` compound assignment added two IEEE bit patterns as integers |
| `1bd85d8` | The suite count, which had gone stale by nine in the one file that holds it |

**Handover 16's REP-56 row is now half-stale.** It says "that read is now bounded
in every profile", which was true of `int_payload` at `b557e0a` and false of
`float_payload`, `praxis_char_load` and `praxis_bool_load` until `af9df6c`.

## What is left, in two lists

They are lists and not a count, for the reason the last section of handover 16
gives.

### Mechanical — one wave, no decisions needed

| Row | Sev | What |
|---|---|---|
| **MIR-10** | P2 | Its verifier landed; its *rule* — "a faulting instruction is followed by a `CheckFault`" — did not. The one §4 finding not marked done |
| **REP-52** | P2 | A fused `collect` pushes with no fault check |
| **REP-53** | P3 | A method call checks unconditionally, so the manifest's fault column has no reader |
| **REP-55** | P3 | `matrix`'s ragged-row fault names the whole input where `grid`'s names the offending line |
| **REP-60** | P2 | Its `--input` half is done; the stdin half needs a runtime contract decision (`praxis_get_input`'s doc states the current behaviour as the design) |

**REP-52 and REP-53 are literally MIR-10's two ends**, and the plan says to land
them together — one change, not three. Adding a `check_fault` at the one
`Sink::Collect` site fixes one site and leaves the class, which is exactly what
the verifier rule exists to prevent.

### Blocked on a language decision, not on effort

| Row | Sev | The question someone has to answer |
|---|---|---|
| **REP-56** | **P0** | The payload record's type does not reach the field read, so `praxis check` exits 0 on a program that then aborts. Needs a real payload-record type — a feature, not a repair |
| **REP-57** | P2 | A record pattern nested in a variant pattern has no grammar. It is REP-56's *workaround*, so it buys nothing until REP-56 is answered |
| **REP-58** | P2 | §7.7's own "repeated labeled blocks" example does not run. Line-anchor a block item's captures, or amend §7.7? |
| **REP-33** | P2 | Appendix D checks clean and does not run: it needs `sorted` and `frequencies`, which §6.3 already defers as barrier combinators |
| **REP-46** | P2 | The `_add` trio landed. Do `_sub` and `_mul` exist? §4.12 names only the three, so nine more rows would be inventing surface |
| **REP-54** | P2 | A template with two or more anonymous captures is tagged `Unit`. The answer is a tuple descriptor from a static path with no tuple constructor, *and* a decision about whether `PlanNode::Tuple` is emitted or deleted — it is currently unreachable from source |

Plus the two open decisions, which have no row because they are not defects:

- **D16** — does `assert` take a message, and more generally does the language get
  arity-based overloading or optional parameters? The plan's warning is the
  important half: `assert`'s message is the cheapest possible motivating case, so
  answering it alone sets the precedent by accident.
- **D18** — may a backtick template span a raw newline? §7.2 does not say, and
  `read \`{int\`` cannot be bounded without answering it. Both candidate rules and
  why neither was taken are in `implementation-repair-progress.md` §5.

## Two sessions, one tree

Mid-session a **second Claude Code session** was started against this repository
on the assumption the first had finished. It is worth knowing what that cost,
because nothing in the repo records it and the symptoms looked like defects:

- A corpus program (`tests/aoc-corpus/m9_grid_and_commands.px`) was modified in
  the working tree with a bare `1 / 0` inserted, which would have turned the gate
  red for reasons no commit explained. It was reverted.
- The two sessions allocated REP numbers and ADR numbers independently. They did
  not collide — ADR-085 was free because the other waves used 082–084, and the
  REP rows interleaved without overlap — but that was luck, not discipline.
- Its work was uncommitted for hours and was committed here, at `9dd3155`, with
  an honest note that the work is its own and the commit is not.

**If two sessions must share a tree, give each its own git worktree.** They are
cheap, `.claude/worktrees/` is now gitignored for this reason, and every
multi-agent wave in this session used them without incident.

## The two things this session would most like the next one to keep

**A gate asserts a value, not an acceptance.** Five tests were labelled gates
this session that passed with their fix entirely removed — the sharpest being
`grid(P, ragged, fill: "x")`, whose test asserted the call was *accepted* while
the fill value was silently dropped, and a graph-oracle test that passed with the
width fix reverted. The rule that came out of it: **a gate is a gate only if it
was observed red with its fix removed, and the observation is written down.**
Tests that cannot be red — characterization tests pinning a move, mutation
companions ruling out a bad fix — are worth having and are listed separately, not
counted.

**A rule stated in four places goes stale in three.** An inverted claim about
ADR-045 and signed zero reached four documents before anyone checked it against
the code (ADR-045 decides the two zeros are *equal* inside a container; it
rejected `f64::total_cmp` precisely for splitting them — verified by running it:
`m[-0.0] = 1` then `m[0.0] = 2` leaves a map of length one). S20's whitespace rule
took five rounds partly because two hand-written scanners each stated it, and the
fix was to delete both and put the rule in `praxis-syntax`, below the crates that
need it. **State a rule where it is enforced and refer to it everywhere else**,
and where a claim is a count, drop the number.
