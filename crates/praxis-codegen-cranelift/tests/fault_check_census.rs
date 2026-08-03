//! How much of the corpus ADR-117 reaches, counted rather than argued.
//!
//! W7's scope was written as *"on `bfs` and `vm` — the two benchmarks dominated
//! by runtime calls — this reaches almost nothing; it is a `collatz`/`primes`
//! change"* (handover 26 §4), and handover 26 §9 registered the number behind
//! that sentence as unmeasured: **how many `Inst::CheckFault`s in the corpus are
//! actually foldable.** This is the measurement, over the eight programs
//! `benchmarks/run.py` runs, and ADR-117 quotes it.
//!
//! **It is a count of sites, not of executions.** A program's hot loop is a few
//! of its blocks and the rest is setup, so a whole-program fraction is an upper
//! bound on nothing and a lower bound on nothing — it says how much of the
//! *written* program the fold reaches. The per-iteration count that carries the
//! headline is in `lower.rs`
//! (`every_fault_check_in_the_sample_loop_is_folded_into_its_raise`), where a
//! named loop makes "per iteration" mean something.
//!
//! **The numbers are asserted, not printed.** A measurement a test only prints
//! is a measurement nobody reads again, and these are quoted in an ADR: if a
//! benchmark is edited the assertion below fails, and updating it *and* the ADR
//! is the whole of the maintenance. That is the intended cost.

use std::path::{Path, PathBuf};

use praxis_mir::ir::{Function, Inst, Overflow};
use praxis_mir::test_support::lower_src_to_mir;

/// The eight benchmark programs, by path, sorted.
fn corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/praxis");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.expect("a directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "px"))
        .collect();
    out.sort();
    out
}

/// `(foldable, total)` `Inst::CheckFault`s in one function.
///
/// Foldable is exactly what `lower::steps` fuses: a check whose immediate
/// predecessor in the same block is `IntBinOp { overflow: Checked }`. The
/// predicate is restated here rather than reached for because `steps` is
/// private to the backend and because a census that shared the implementation
/// under test would agree with it by construction.
fn foldable(func: &Function) -> (usize, usize) {
    let mut fold = 0;
    let mut total = 0;
    for block in &func.blocks {
        for (i, inst) in block.insts.iter().enumerate() {
            if !matches!(inst, Inst::CheckFault { .. }) {
                continue;
            }
            total += 1;
            let previous = i.checked_sub(1).and_then(|p| block.insts.get(p));
            if matches!(
                previous,
                Some(Inst::IntBinOp {
                    overflow: Overflow::Checked,
                    ..
                })
            ) {
                fold += 1;
            }
        }
    }
    (fold, total)
}

/// `(foldable, total)` over every function one program lowers to.
fn census(path: &Path) -> (usize, usize) {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    lower_src_to_mir(&src)
        .funcs
        .iter()
        .map(foldable)
        .fold((0, 0), |(f, t), (df, dt)| (f + df, t + dt))
}

/// Just over half the corpus's fault checks are foldable, and `vm` is the
/// program that says why the geometric mean is the wrong headline.
///
/// | program | foldable / total |
/// |---|---:|
/// | `bfs` | 39 / 60 |
/// | `collatz` | 6 / 8 |
/// | `hashwork` | 15 / 18 |
/// | `mandelbrot` | 4 / 6 |
/// | `pipeline` | 21 / 30 |
/// | `primes` | 7 / 10 |
/// | `tree` | 21 / 32 |
/// | `vm` | **5 / 58** |
/// | **total** | **118 / 222** |
///
/// `vm` is a `match` over ten opcodes with a `Deque` under it: nearly every
/// fallible thing it does is a wrapper call, and a wrapper call's check is the
/// one shape that cannot fold, because the wrapper sets the slot and *returns*.
#[test]
fn just_over_half_the_corpus_fault_checks_are_foldable() {
    let measured: Vec<(String, usize, usize)> = corpus()
        .iter()
        .map(|p| {
            let (f, t) = census(p);
            (p.file_stem().unwrap().to_string_lossy().into_owned(), f, t)
        })
        .collect();
    let expected = [
        ("bfs", 39, 60),
        ("collatz", 6, 8),
        ("hashwork", 15, 18),
        ("mandelbrot", 4, 6),
        ("pipeline", 21, 30),
        ("primes", 7, 10),
        ("tree", 21, 32),
        ("vm", 5, 58),
    ];
    let got: Vec<(&str, usize, usize)> = measured
        .iter()
        .map(|(n, f, t)| (n.as_str(), *f, *t))
        .collect();
    assert_eq!(
        got,
        expected.to_vec(),
        "the corpus census moved; ADR-117's table is quoted from it and has to \
         move with it"
    );
    let (fold, total) = measured
        .iter()
        .fold((0, 0), |(f, t), (_, df, dt)| (f + df, t + dt));
    assert_eq!((fold, total), (118, 222));
}

/// `vm` is the honest counter-example, and it is worth its own name: the fold
/// reaches under a tenth of its checks. A package that reported a suite mean
/// would bury this.
#[test]
fn the_fold_reaches_under_a_tenth_of_vms_fault_checks() {
    let vm = corpus()
        .into_iter()
        .find(|p| p.file_stem().is_some_and(|s| s == "vm"))
        .expect("`vm.px` is in the corpus");
    let (fold, total) = census(&vm);
    assert!(
        fold * 10 < total,
        "`vm` folds {fold} of {total} checks, which is not the under-a-tenth \
         this test is named for"
    );
}
