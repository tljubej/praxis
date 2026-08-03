#!/usr/bin/env python3
"""Per-iteration instruction counts off a `praxis-dump`, by `dump.rs`'s rule.

**Why this is in the tree rather than in a scratchpad someone rewrites.** Wave 0
put `PRAXIS_DUMP_CLIF` / `PRAXIS_DUMP_VCODE` in the tree so that five packages
quoting an instruction count would not each add and revert the same `eprintln`
(ADR-105, and `dump.rs`'s own module doc). The *walk* that turns a dump into a
per-iteration number is one level up from the hook, and one round later **two
separately hand-written walkers of the same rule were wrong in two different
ways** — which is the argument for the hook, made again.

The first read each IR's per-block counts off that IR's own header line —
correct — and then walked **the CLIF control-flow graph for both**. `dump.rs`
had already written down why that cannot work: "the block numbers are the
*lowered* block order and are not the CLIF block numbering… the two dumps are
comparable in total, not row by row." On handover 25 §3's loop at `2491140` the
mistake is worth **15 machine instructions per iteration, 130 against the true
115**, and it is in the same direction for both arms of an A/B, so it survives
subtraction quietly and only shows up in a headline. ADR-118 part 2 caught it.

The second built each IR's graph correctly and then had **no set of cold vcode
blocks at all**, and took the first in-component successor the listing named. The
vcode listing marks nothing `cold`; coldness survives into it only as emission
order. So wherever Cranelift emits the out-of-line wrapper call before the inline
arm, that walk went *through* the wrapper and counted it as part of the
iteration. It agrees with the truth on any path where no branch names a cold
block first, which is what made it hard to see — three of five rows of ADR-120
part 2's table are right and two are not.

**The rule, from `dump.rs`'s module doc.** The loop is the one strongly
connected component of the CFG with more than one block; its header is the
member emitted first; from there, at each branch, take the successor that is
inside the component and is not cold.

Every clause below is a clause because a count came out wrong without it:

* **Each IR is walked over its own CFG.** CLIF successors are `blockN` operands
  of `brif`/`jump`; vcode successors are `labelN` operands of the emitted
  branches. Nothing in this file ever reads one IR's graph while summing the
  other's counts. `the_two_irs_are_walked_over_their_own_graphs` is the
  regression test, and its fixture permutes the numbering exactly the way a real
  lowering does, so the wrong walk yields a wrong number rather than a crash.

* **The `prologue` row is not a block.** The vcode breakdown carries one entry
  for the instructions before the first label; they are real instructions and
  are in the total, but they belong to no lowered block and have no place in a
  loop body. `dump.rs` reports "N blocks + prologue" for this reason and this
  file drops the row from the block set rather than letting it be a node with
  no edges.

* **Cold blocks are excluded, and the two IRs say so differently.** CLIF marks
  them (`block8 cold:`). The vcode listing does not mark them at all: coldness
  survives into it as *emission order*, because Cranelift lays out every hot
  block and then every cold block, each group in the lowered order, so the
  breakdown's block ids ascend, drop once, and ascend again. That single descent
  is the boundary — asserted, not assumed, so that a Cranelift bump which
  changed the layout would fail here loudly instead of moving every count by the
  size of an out-of-line wrapper call. Cold blocks are genuinely *inside* the
  loop's component (`block8 cold` calls the wrapper and jumps back to `block9`),
  so this is not a distinction that can be skipped.

* **A branch may have more than one hot successor, and this tool never picks
  one.** ADR-118 part 2 found the first case: `bs.contains(x)`'s `absent` arm is
  a *correct answer*, not a bail-out, so it is hot and in the component beside
  the `read` arm, and such a loop has four hot cycles rather than one. A walker
  that took the first successor would report one of the four and call it "the"
  per-iteration count. This one enumerates every simple hot cycle through the
  header and prints the whole spread; a caller that wants a single number must
  say which cycle it means.

* **It self-tests** (`--self-test`), on fixtures small enough to check by hand,
  because the two walkers it replaces were wrong for a whole round without anyone
  being able to tell by reading their output. The other half of that check is a
  recorded answer: this tool reproduces Wave 0's baseline — 311 CLIF in 55 blocks
  and 171 over 35, 458 vcode in 67 blocks + prologue and 215 over 38, from
  `dump.rs`'s module doc — to the instruction, and that pair predates every
  walker in the round and has two different denominators.

Usage:

    periter.py DUMP [DUMP …]            both IRs in each dump
    periter.py DUMP --ir vcode          one IR
    periter.py DUMP --function '<entry>'
    periter.py --self-test

A DUMP is the stderr of a run with the dump hooks on:

    PRAXIS_DUMP_CLIF='<entry>' PRAXIS_DUMP_VCODE='<entry>' \\
        praxis run loop.px 2>dump.txt
"""

import argparse
import re
import sys
from collections import defaultdict

# The two header lines `dump.rs::summarize` writes. They share a prefix, so the
# totals line is told from the breakdown line by " instructions in ", not by
# position — a dump with a missing disassembly writes a one-line record and
# position would then be off by one for everything after it.
TOTALS = re.compile(
    r"^;; praxis-dump (clif|vcode) `(.+)`: (\d+) instructions in (\d+) blocks"
    r"(?: \+ prologue)?(?:, (\d+) bytes of machine code)?$"
)
BREAKDOWN = re.compile(r"^;; praxis-dump (clif|vcode) `(.+)`: ((?:\w+=\d+ ?)+)$")
ANY_DUMP_LINE = re.compile(r"^;; praxis-dump ")

# A CLIF block label is at column zero, may carry block parameters, and may
# carry ` cold`. A vcode label is bare. Both end the line at the colon.
CLIF_LABEL = re.compile(r"^block(\d+)(?:\([^)]*\))?( cold)?:$")
VCODE_LABEL = re.compile(r"^block(\d+):$")

# Successor operands. These are deliberately different tokens: `block7` in CLIF,
# `label7` in vcode. Reading one IR's body with the other's pattern finds
# nothing, which is the failure mode this file exists to make impossible.
CLIF_EDGE = re.compile(r"\bblock(\d+)\b")
VCODE_EDGE = re.compile(r"\blabel(\d+)\b")

# A loop with more hot cycles than this is not something anyone is going to read
# off a printed list, and enumerating simple cycles is exponential in the worst
# case. Refuse rather than hang.
CYCLE_LIMIT = 64


class DumpError(Exception):
    """A dump this tool will not guess about."""


class Record:
    """One IR of one function: its header numbers, its breakdown, its body."""

    def __init__(self, kind, name, total, blocks, code_bytes, order, counts, body):
        self.kind = kind
        self.name = name
        self.total = total
        self.blocks = blocks
        self.code_bytes = code_bytes
        # `order` is emission order — `func.layout.blocks()` for CLIF, listing
        # order for vcode — and both the loop header ("the member emitted
        # first") and the vcode cold boundary are read out of it.
        self.order = order
        self.counts = counts
        self.body = body

    @property
    def prologue(self):
        return self.counts.get("prologue", 0)


def parse(text):
    """Every `praxis-dump` record in one stderr capture, in the order written."""
    lines = text.splitlines()
    records, pending, body = [], None, []

    def flush():
        if pending is not None:
            pending.body = "\n".join(body)
            records.append(pending)

    i = 0
    while i < len(lines):
        totals = TOTALS.match(lines[i].rstrip())
        if not totals:
            if pending is not None:
                body.append(lines[i])
            i += 1
            continue
        flush()
        kind, name, total, blocks, code_bytes = totals.groups()
        i += 1
        breakdown = BREAKDOWN.match(lines[i].rstrip()) if i < len(lines) else None
        if breakdown is None or breakdown.group(1) != kind:
            raise DumpError(
                f"the {kind} record for `{name}` has no per-block breakdown line "
                "after its totals line"
            )
        order, counts = [], {}
        for cell in breakdown.group(3).split():
            label, n = cell.split("=")
            order.append(label)
            counts[label] = int(n)
        pending = Record(
            kind,
            name,
            int(total),
            int(blocks),
            int(code_bytes) if code_bytes else None,
            order,
            counts,
            "",
        )
        body = []
        i += 1
    flush()
    if not records:
        raise DumpError(
            "no `;; praxis-dump` records here — was PRAXIS_DUMP_CLIF or "
            "PRAXIS_DUMP_VCODE set, and was stderr captured?"
        )
    return records


def cfg(record):
    """`(successors, cold)` over the record's own body, keyed by block id."""
    label, edge = (
        (CLIF_LABEL, CLIF_EDGE) if record.kind == "clif" else (VCODE_LABEL, VCODE_EDGE)
    )
    succ, cold, cur = defaultdict(list), set(), None
    for line in record.body.splitlines():
        if not line.startswith(" ") and line.rstrip():
            m = label.match(line.rstrip())
            if m:
                cur = f"block{m.group(1)}"
                succ.setdefault(cur, [])
                if record.kind == "clif" and m.group(2):
                    cold.add(cur)
                continue
        if cur is None:
            # The CLIF preamble (`sig0 = …`, `fn0 = …`) and the vcode prologue,
            # neither of which belongs to a block.
            continue
        for t in edge.findall(line):
            if f"block{t}" not in succ[cur]:
                succ[cur].append(f"block{t}")

    labelled = set(succ)
    counted = {b for b in record.counts if b != "prologue"}
    if labelled != counted:
        raise DumpError(
            f"the {record.kind} body and its breakdown disagree about which "
            f"blocks exist: body-only {sorted(labelled - counted)}, "
            f"breakdown-only {sorted(counted - labelled)}"
        )
    if record.kind == "vcode":
        cold = cold_by_emission(record)
    return succ, cold


def cold_by_emission(record):
    """Cold vcode blocks: the suffix of emission order after the one descent.

    Cranelift emits every hot block and then every cold block, each group in
    the lowered order that numbered them, so the breakdown's ids ascend, drop
    once, and ascend again. Nothing in the listing says `cold`, so this is the
    only signal there is — and it is asserted rather than trusted, because a
    layout change upstream would otherwise silently move every per-iteration
    count by the size of an out-of-line wrapper call.
    """
    ids = [int(b[len("block") :]) for b in record.order if b != "prologue"]
    descents = [i for i in range(1, len(ids)) if ids[i] < ids[i - 1]]
    if not descents:
        return set()
    if len(descents) > 1:
        raise DumpError(
            "the vcode emission order descends "
            f"{len(descents)} times, at {descents}, so hot-then-cold is no "
            "longer the layout and coldness cannot be read off it: "
            f"{[f'block{i}' for i in ids]}"
        )
    return {f"block{i}" for i in ids[descents[0] :]}


def sccs(nodes, succ):
    """Tarjan, iterative — these graphs are small but recursion depth is not."""
    index, low, on_stack, stack, out = {}, {}, set(), [], []
    counter = 0
    for root in nodes:
        if root in index:
            continue
        work = [(root, iter(succ.get(root, ())))]
        index[root] = low[root] = counter
        counter += 1
        stack.append(root)
        on_stack.add(root)
        while work:
            node, it = work[-1]
            child = next(it, None)
            if child is None:
                work.pop()
                if low[node] == index[node]:
                    comp = []
                    while True:
                        w = stack.pop()
                        on_stack.discard(w)
                        comp.append(w)
                        if w == node:
                            break
                    out.append(set(comp))
                if work:
                    parent = work[-1][0]
                    low[parent] = min(low[parent], low[node])
                continue
            if child not in index:
                index[child] = low[child] = counter
                counter += 1
                stack.append(child)
                on_stack.add(child)
                work.append((child, iter(succ.get(child, ()))))
            elif child in on_stack:
                low[node] = min(low[node], index[child])
    return out


def loop_component(record, succ):
    """The one component `dump.rs`'s rule names, or an error saying why not.

    "More than one block" is `dump.rs`'s wording and it is the common case, but
    a single block with an edge to itself is a loop too and is counted here —
    stating the rule in terms of back edges rather than size costs nothing and
    means a fully collapsed loop body does not read as "no loop found".
    """
    order = [b for b in record.order if b != "prologue"]
    cyclic = [
        c
        for c in sccs(order, succ)
        if len(c) > 1 or any(b in succ.get(b, ()) for b in c)
    ]
    if len(cyclic) != 1:
        raise DumpError(
            f"{record.kind} `{record.name}` has {len(cyclic)} cyclic components, "
            f"not one: {[sorted(c) for c in cyclic]}. `dump.rs`'s rule names the "
            "single loop of a microbenchmark; a function with two loops needs a "
            "rule that says which."
        )
    return cyclic[0]


def hot_cycles(header, comp, cold, succ):
    """Every simple cycle from the header through hot in-component blocks.

    Not "the" cycle. ADR-118 part 2's `contains` loop has four, because an
    `absent` answer is as correct as a `read` and both arms are hot, and a
    walker that took `succ[0]` would have reported one of the four as the count.
    """
    found = []

    def step(node, sofar):
        if len(found) > CYCLE_LIMIT:
            return
        for s in succ.get(node, ()):
            if s not in comp or s in cold:
                continue
            if s == header:
                found.append(list(sofar))
            elif s not in sofar:
                step(s, sofar + [s])

    step(header, [header])
    if not found:
        raise DumpError(
            f"no hot cycle returns to the loop header {header}; the component is "
            f"{sorted(comp)} and its cold members are {sorted(comp & cold)}"
        )
    if len(found) > CYCLE_LIMIT:
        raise DumpError(
            f"more than {CYCLE_LIMIT} hot cycles through {header}; this is not a "
            "microbenchmark loop and a printed list of paths is not the answer"
        )
    return found


def analyse(record):
    """Everything `report` prints, so a caller can have the numbers instead."""
    succ, cold = cfg(record)
    comp = loop_component(record, succ)
    header = next(b for b in record.order if b in comp)
    cycles = hot_cycles(header, comp, cold, succ)
    totals = sorted((sum(record.counts[b] for b in c), c) for c in cycles)
    return {
        "record": record,
        "cold": cold,
        "component": comp,
        "header": header,
        "cycles": totals,
    }


def report(record, out=sys.stdout):
    a = analyse(record)
    prologue = f" + prologue={record.prologue}" if record.prologue else ""
    size = f", {record.code_bytes} bytes" if record.code_bytes else ""
    print(
        f"{record.kind} `{record.name}`: whole function {record.total} in "
        f"{record.blocks} blocks{prologue}{size}; "
        f"{record.blocks - len(a['cold'])} hot, {len(a['cold'])} cold",
        file=out,
    )
    print(
        f"  loop: {len(a['component'])} blocks, header {a['header']}, "
        f"{len(a['component'] & a['cold'])} of them cold",
        file=out,
    )
    if len(a["cycles"]) == 1:
        total, path = a["cycles"][0]
        print(f"  per iteration: {total} over {len(path)} blocks", file=out)
    else:
        lo, hi = a["cycles"][0], a["cycles"][-1]
        print(
            f"  per iteration: {len(a['cycles'])} hot cycles, "
            f"{lo[0]} over {len(lo[1])} blocks .. {hi[0]} over {len(hi[1])} "
            "blocks — no single number is the answer here",
            file=out,
        )
    for total, path in a["cycles"]:
        print(f"    {total:>5}  {' '.join(path)}", file=out)
    return a


# --------------------------------------------------------------------------
# Fixtures. Each is the smallest dump that makes one clause fail if it is
# dropped, and each is checkable by hand from the numbers in its breakdown.
# --------------------------------------------------------------------------

# CLIF and vcode for the same three-block loop, numbered *differently* — which
# is what a real lowering does (`dump.rs`: "a CLIF block can be split, dropped
# or reordered before it reaches here"). CLIF's loop is {block1, block2}; the
# same loop in vcode is {block1, block3}. Walking vcode counts over the CLIF
# component gives 5 + 4 = 9; the truth is 5 + 7 = 12.
PERMUTED = """\
;; praxis-dump clif `f`: 12 instructions in 4 blocks
;; praxis-dump clif `f`: block0=2 block1=3 block2=4 block3=3
function u0:0(i64) -> i64 fast {
block0(v0: i64):
    v1 = iconst.i64 0
    jump block1

block1:
    v2 = icmp slt v1, v0
    brif v2, block2, block3

block2:
    v3 = iconst.i64 1
    v4 = iadd.i64 v1, v3
    v5 = iadd.i64 v4, v3
    jump block1

block3:
    v6 = iconst.i64 0
    v7 = iadd.i64 v6, v6
    return v7
}
;; praxis-dump vcode `f`: 18 instructions in 4 blocks, 72 bytes of machine code
;; praxis-dump vcode `f`: block0=2 block1=5 block2=4 block3=7
block0:
  movz x1, #0
  b label1
block1:
  subs xzr, x1, x0
  cset x2, lt
  uxtb w2, w2
  cbz x2, label2 ; b label3
  nop
block2:
  movz x3, #0
  add x0, x3, x3
  mov x0, x0
  ret
block3:
  movz x4, #1
  add x1, x1, x4
  add x1, x1, x4
  mov x5, x1
  mov x1, x5
  nop
  b label1
"""

# A vcode dump with a prologue and a cold block. Emission order is
# 0, 1, 3 then 2 — one descent, so block2 is cold. The loop component is
# {block1, block3, block2}: the cold block jumps *back* into the loop, exactly
# as a wrapper call does, so "in the component" does not mean "on the path".
# Per iteration is block1 + block3 = 6 + 4 = 10; the prologue's 9 and the cold
# block's 20 are in the whole-function total and out of the loop body.
PROLOGUE_AND_COLD = """\
;; praxis-dump vcode `g`: 42 instructions in 4 blocks + prologue, 168 bytes of machine code
;; praxis-dump vcode `g`: prologue=9 block0=3 block1=6 block3=4 block2=20
  pacibsp
  stp fp, lr, [sp, #-16]!
  mov fp, sp
  stp x27, x28, [sp, #-16]!
  stp x25, x26, [sp, #-16]!
  stp x23, x24, [sp, #-16]!
  stp x21, x22, [sp, #-16]!
  stp x19, x20, [sp, #-16]!
  sub sp, sp, #16
block0:
  movz x1, #0
  movz x2, #0
  b label1
block1:
  ldr x3, [x1]
  ldr x4, [x0, #152]
  subs xzr, x3, x4
  cset x5, eq
  uxtb w5, w5
  cbz x5, label2 ; b label3
block3:
  ldr x6, [x1, #16]
  add x2, x2, x6
  subs xzr, x2, x0
  b.lt label1 ; b label9
block2:
  mov x0, x0
  mov x1, x1
  bl 0
  mov x6, x0
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  mov x6, x6
  b label3
"""

# ADR-118 part 2's shape: `bs.contains(x)` answers `absent` as correctly as it
# answers `read`, so both arms are hot and both are in the loop. Two `contains`
# in one body is therefore 2 × 2 = **four** hot cycles, not one. The blocks
# outside the two diamonds are 1 + 2 + 2 + 3 = 8, and the arms are 4-or-5 and
# 3-or-7, so the four answers are 15, 16, 19 and 20. A walker taking the first
# in-component successor would print one of them and call it the count.
TWO_HOT_ARMS = """\
;; praxis-dump clif `h`: 32 instructions in 10 blocks
;; praxis-dump clif `h`: block0=1 block1=1 block2=2 block3=4 block4=4 block5=5 \
block6=2 block7=3 block8=7 block9=3
function u0:0(i64) -> i64 fast {
block0(v0: i64):
    jump block1

block1:
    brif v0, block2, block3

block2:
    v1 = load.i64 notrap aligned v0
    brif v1, block4, block5

block3:
    v2 = iconst.i64 2
    v3 = iadd.i64 v2, v2
    v4 = iadd.i64 v3, v3
    return v4

block4:
    v5 = iconst.i64 3
    v6 = iadd.i64 v5, v5
    v7 = iadd.i64 v6, v6
    jump block6

block5:
    v8 = iconst.i64 4
    v9 = iadd.i64 v8, v8
    v10 = iadd.i64 v9, v9
    v11 = iadd.i64 v10, v10
    jump block6

block6:
    v12 = load.i64 notrap aligned v0+8
    brif v12, block7, block8

block7:
    v13 = iconst.i64 5
    v14 = iadd.i64 v13, v13
    jump block9

block8:
    v15 = iconst.i64 6
    v16 = iadd.i64 v15, v15
    v17 = iadd.i64 v16, v16
    v18 = iadd.i64 v17, v17
    v19 = iadd.i64 v18, v18
    v20 = iadd.i64 v19, v19
    jump block9

block9:
    v21 = iadd.i64 v0, v0
    v22 = iadd.i64 v21, v21
    jump block1
}
"""

# The vcode layout assertion: emission order 0, 3, 1, 4, 2 descends twice, so
# it is not hot-then-cold and this tool must refuse rather than guess which
# suffix is the cold one.
TWO_DESCENTS = """\
;; praxis-dump vcode `k`: 10 instructions in 5 blocks
;; praxis-dump vcode `k`: block0=2 block3=2 block1=2 block4=2 block2=2
block0:
  subs xzr, x1, x0
  cbz x1, label3 ; b label1
block3:
  mov x0, x1
  ret
block1:
  subs xzr, x1, x0
  cbz x1, label4 ; b label2
block4:
  mov x0, x0
  ret
block2:
  add x1, x1, x1
  b label1
"""


def self_test():
    """Every clause of the docstring, as a check that fails without it."""
    failures = []

    def check(name, thunk):
        try:
            thunk()
        except AssertionError as exc:
            failures.append(f"{name}: {exc}")
        except Exception as exc:  # noqa: BLE001 — a raise here is a failure too
            failures.append(f"{name}: unexpected {type(exc).__name__}: {exc}")
        else:
            print(f"  ok  {name}")

    def the_two_irs_are_walked_over_their_own_graphs():
        clif, vcode = parse(PERMUTED)
        a_clif, a_vcode = analyse(clif), analyse(vcode)
        assert a_clif["component"] == {"block1", "block2"}, a_clif["component"]
        assert a_vcode["component"] == {"block1", "block3"}, a_vcode["component"]
        assert a_clif["cycles"][0][0] == 7, a_clif["cycles"]
        assert a_vcode["cycles"][0][0] == 12, a_vcode["cycles"]
        # The defect, spelled out: the same vcode counts summed over the CLIF
        # component. It does not crash and it is not 12.
        through_the_wrong_graph = sum(
            vcode.counts[b] for b in a_clif["component"]
        )
        assert through_the_wrong_graph == 9, through_the_wrong_graph

    def a_prologue_is_not_a_block_and_never_reaches_the_loop_body():
        (vcode,) = parse(PROLOGUE_AND_COLD)
        assert vcode.blocks == 4, vcode.blocks
        assert vcode.total == 42, vcode.total
        a = analyse(vcode)
        assert "prologue" not in a["component"], a["component"]
        assert a["cycles"][0][0] == 10, a["cycles"]

    def a_cold_block_inside_the_loop_is_in_the_component_and_off_the_path():
        (vcode,) = parse(PROLOGUE_AND_COLD)
        a = analyse(vcode)
        assert a["cold"] == {"block2"}, a["cold"]
        assert "block2" in a["component"], a["component"]
        assert a["cycles"][0][1] == ["block1", "block3"], a["cycles"]

    def a_clif_cold_marker_is_read_off_the_label():
        (clif,) = parse(
            ";; praxis-dump clif `c`: 6 instructions in 3 blocks\n"
            ";; praxis-dump clif `c`: block0=2 block1=2 block2=2\n"
            "block0(v0: i64):\n"
            "    brif v0, block1, block2\n"
            "\n"
            "block1 cold:\n"
            "    v1 = iconst.i64 1\n"
            "    jump block0\n"
            "\n"
            "block2:\n"
            "    v2 = iconst.i64 2\n"
            "    jump block0\n"
        )
        succ, cold = cfg(clif)
        assert cold == {"block1"}, cold
        assert analyse(clif)["cycles"][0][1] == ["block0", "block2"]

    def two_hot_successors_are_both_reported_and_neither_is_picked():
        (clif,) = parse(TWO_HOT_ARMS)
        a = analyse(clif)
        totals = [t for t, _ in a["cycles"]]
        assert totals == [15, 16, 19, 20], totals

    def a_vcode_layout_that_is_not_hot_then_cold_is_refused():
        (vcode,) = parse(TWO_DESCENTS)
        try:
            analyse(vcode)
        except DumpError as exc:
            assert "descends 2 times" in str(exc), exc
        else:
            raise AssertionError("two descents were accepted as hot-then-cold")

    def a_body_and_a_breakdown_that_disagree_are_refused():
        (clif,) = parse(
            ";; praxis-dump clif `d`: 4 instructions in 2 blocks\n"
            ";; praxis-dump clif `d`: block0=2 block9=2\n"
            "block0(v0: i64):\n"
            "    v1 = iconst.i64 1\n"
            "    jump block0\n"
        )
        try:
            cfg(clif)
        except DumpError as exc:
            assert "disagree about which blocks exist" in str(exc), exc
        else:
            raise AssertionError("a breakdown naming a block the body lacks passed")

    def a_self_loop_is_a_loop_even_though_it_is_one_block():
        (clif,) = parse(
            ";; praxis-dump clif `s`: 5 instructions in 2 blocks\n"
            ";; praxis-dump clif `s`: block0=3 block1=2\n"
            "block0(v0: i64):\n"
            "    v1 = iconst.i64 1\n"
            "    v2 = iadd.i64 v0, v1\n"
            "    brif v2, block0, block1\n"
            "\n"
            "block1:\n"
            "    v3 = iconst.i64 0\n"
            "    return v3\n"
        )
        a = analyse(clif)
        assert a["component"] == {"block0"}, a["component"]
        assert a["cycles"] == [(3, ["block0"])], a["cycles"]

    def both_records_of_one_capture_are_parsed_in_order():
        records = parse(PERMUTED)
        assert [r.kind for r in records] == ["clif", "vcode"], records
        assert records[1].code_bytes == 72, records[1].code_bytes
        assert records[0].code_bytes is None, records[0].code_bytes

    for fn in (
        the_two_irs_are_walked_over_their_own_graphs,
        a_prologue_is_not_a_block_and_never_reaches_the_loop_body,
        a_cold_block_inside_the_loop_is_in_the_component_and_off_the_path,
        a_clif_cold_marker_is_read_off_the_label,
        two_hot_successors_are_both_reported_and_neither_is_picked,
        a_vcode_layout_that_is_not_hot_then_cold_is_refused,
        a_body_and_a_breakdown_that_disagree_are_refused,
        a_self_loop_is_a_loop_even_though_it_is_one_block,
        both_records_of_one_capture_are_parsed_in_order,
    ):
        check(fn.__name__, fn)

    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("self-test: all clauses hold")
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("dumps", nargs="*", help="files holding a run's stderr")
    ap.add_argument(
        "--ir",
        choices=("clif", "vcode", "both"),
        default="both",
        help="which IR to walk (default: both)",
    )
    ap.add_argument("--function", help="only this function's records")
    ap.add_argument("--self-test", action="store_true", help="check every clause")
    args = ap.parse_args(argv)

    if args.self_test:
        return self_test()
    if not args.dumps:
        ap.error("a dump file, or --self-test")

    status = 0
    for path in args.dumps:
        print(path)
        with open(path) as f:
            text = f.read()
        try:
            records = parse(text)
        except DumpError as exc:
            print(f"  {exc}", file=sys.stderr)
            status = 1
            continue
        for record in records:
            if args.ir != "both" and record.kind != args.ir:
                continue
            if args.function and record.name != args.function:
                continue
            try:
                report(record)
            except DumpError as exc:
                print(f"  {record.kind} `{record.name}`: {exc}", file=sys.stderr)
                status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
