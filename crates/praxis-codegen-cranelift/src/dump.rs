//! `PRAXIS_DUMP_CLIF` and `PRAXIS_DUMP_VCODE`: the code the backend actually
//! emitted, on stderr, out of the real compile path.
//!
//! **Why this is in the tree rather than in a diff someone keeps re-applying.**
//! Handover 25 §3 priced the whole optimization plan off one measurement — 156
//! CLIF instructions and 216 aarch64 instructions per iteration of a
//! two-statement loop, four of which were the arithmetic — and that number came
//! from an `eprintln` added by hand and reverted with the branch. Five of the
//! packages in handover 26's plan (W4b, W6, W7, W10, W12) state their headline
//! as an instruction count, because a change that removes three instructions
//! from a loop body is a *deterministic* result the clock on this laptop cannot
//! resolve (26 §6). Without a hook each of them adds and reverts the same edit
//! to the same file, and no two of their counts are comparable.
//!
//! **Stderr, and that is load-bearing.** The A/B protocol diffs a benchmark's
//! stdout byte-for-byte between the two arms and voids the measurement when it
//! differs (26 §6). A dump on stdout would void exactly the runs it exists to
//! explain.
//!
//! **The counts come with the dump.** Counting instructions out of raw CLIF by
//! hand is how the 156/216 figures ended up unreproducible in the first place;
//! the header lines below are the answer, per block, so a later package quotes
//! one line instead of a scroll of IR.
//!
//! # The baseline, and how a per-iteration count is read off a dump
//!
//! Handover 25 §3's loop, which is the program every instruction count in
//! handover 26's plan is quoted against:
//!
//! ```text
//! var i = 0
//! var acc = 0
//! let limit = 10
//! while i < limit {
//!     acc = acc + i * 3
//!     i = i + 1
//! }
//! out(acc)
//! ```
//!
//! At `1535eb6`, `PRAXIS_DUMP_CLIF='<entry>'` and `PRAXIS_DUMP_VCODE='<entry>'`
//! through `praxis run`:
//!
//! | | whole function | per iteration |
//! |---|---:|---:|
//! | CLIF | 311 in 55 blocks | **171** over 35 blocks |
//! | vcode | 458 in 67 blocks + prologue, 1960 bytes | **215** over 38 blocks |
//!
//! **The two block counts are the same denominator**, which they were not until
//! this note was written: the vcode header used to say 68 because it counted the
//! synthetic `prologue` entry as a block. The prologue is the instructions before
//! the first label and belongs to no lowered block, so it is now reported beside
//! the count rather than inside it. The instruction totals were always right.
//!
//! A **per-iteration** count is a walk over the per-block header line, and the
//! rule is: the loop is the one strongly connected component of the CFG with
//! more than one block; its header is the member emitted first; from there take,
//! at each branch, the successor that is inside the component and is not cold.
//! The fault epilogue and the loop exit fall out because they return, so they
//! are outside the component; the out-of-line wrapper calls fall out because
//! they are marked `cold` in the CLIF and are emitted after every hot block in
//! the vcode. Handover 25's own figures for the same shape were 156 CLIF and
//! **216** aarch64 — the second is one instruction from what this reproduces,
//! which is as much agreement as two hand-written copies of a loop can give.

use std::sync::OnceLock;

use cranelift::codegen::ir::Function;
use cranelift::prelude::codegen;

/// The environment variable selecting the Cranelift IR dump. That IR is what
/// the builder emitted with `Context::optimize`'s `opt_level = "none"` tidying
/// applied and nothing more — see [`emit`], which is where the exact list is.
const CLIF_VAR: &str = "PRAXIS_DUMP_CLIF";

/// The environment variable selecting the machine-level vcode dump.
const VCODE_VAR: &str = "PRAXIS_DUMP_VCODE";

/// Which functions one `PRAXIS_DUMP_*` variable selects.
///
/// A whole-program dump is the wrong default for anything but a one-function
/// microbenchmark — `<entry>` alone is ~470 instructions (handover 25 §3), and
/// the interesting question is almost always about one loop in one function. So
/// the variable takes names as well as `1`, and the by-name form is what the
/// measurement packages should use.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DumpSelection {
    /// Unset, empty, or a list that named nothing. Costs one branch per
    /// compiled function and nothing else.
    Nothing,
    /// `1` or `all`: every function this process compiles, including the ones
    /// the debugger mints for `p EXPR`.
    Every,
    /// A comma-separated list of MIR function names — `<entry>`, `main`, a
    /// user `fn`. Empty entries are dropped at parse time so that this and
    /// [`DumpSelection::Nothing`] never encode the same answer.
    Named(Vec<String>),
}

impl DumpSelection {
    /// Read one variable's value. `None` is an unset variable.
    fn parse(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::Nothing;
        };
        let value = value.trim();
        if value.is_empty() {
            return Self::Nothing;
        }
        if value == "1" || value.eq_ignore_ascii_case("all") {
            return Self::Every;
        }
        let names: Vec<String> = value
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect();
        if names.is_empty() {
            Self::Nothing
        } else {
            Self::Named(names)
        }
    }

    /// Does this selection cover the function called `name`?
    fn covers(&self, name: &str) -> bool {
        match self {
            Self::Nothing => false,
            Self::Every => true,
            Self::Named(names) => names.iter().any(|n| n == name),
        }
    }
}

/// Both hooks, as this process resolved them.
#[derive(Debug)]
struct DumpHooks {
    clif: DumpSelection,
    vcode: DumpSelection,
}

impl DumpHooks {
    /// Is either dump asked for, for this function? The one question the
    /// compile path asks on every function it compiles.
    fn selects(&self, name: &str) -> bool {
        self.clif.covers(name) || self.vcode.covers(name)
    }
}

/// This process's hooks, read from the environment **once**.
///
/// Not once per function and not once per [`Jit`](crate::Jit): `Jit::compile`
/// lowers every function of a program through one call, and the debugger mints
/// a fresh `Jit` for every `p EXPR` in a session. A `OnceLock` makes the cost of
/// an unset variable one relaxed load and one branch per compiled function,
/// which is nothing against a Cranelift compilation, and makes the answer
/// stable for a process that is mid-run.
fn hooks() -> &'static DumpHooks {
    static HOOKS: OnceLock<DumpHooks> = OnceLock::new();
    HOOKS.get_or_init(|| DumpHooks {
        clif: DumpSelection::parse(std::env::var(CLIF_VAR).ok().as_deref()),
        vcode: DumpSelection::parse(std::env::var(VCODE_VAR).ok().as_deref()),
    })
}

/// Does this function need `Context::set_disasm(true)` before it is defined?
///
/// The disassembly is only produced if it was requested *before* compilation —
/// `Context::compile_stencil` passes `want_disasm` down to the ISA and there is
/// no way to ask afterwards — so this question and the one [`emit`] answers
/// must agree. They do by construction: both are `self.vcode.covers(name)`.
pub(crate) fn wants_vcode(name: &str) -> bool {
    hooks().vcode.covers(name)
}

/// Write whatever the hooks ask for about the function just defined.
///
/// **Called between `define_function` and `clear_context`**, which is the only
/// window in which either text exists: `clear_context` drops both the function
/// and the compiled code on the next line.
///
/// **What `define_function` did to `ctx.func` first, and it is less than
/// "optimized".** It reaches `Context::compile` → `compile_stencil` →
/// `Context::optimize`, and at this JIT's `opt_level = "none"`
/// ([`CRANELIFT_FLAGS`](crate::module::CRANELIFT_FLAGS)) that whole pass reduces
/// to unreachable-block elimination, constant-block-parameter removal
/// (`remove_constant_phis`) and `resolve_all_aliases`. The mid-end egraph pass
/// — the one that would fold, GVN and license the word *optimized* — is behind
/// `if opt_level != OptLevel::None` and does not run; nor does NaN
/// canonicalization, whose flag defaults off. There is no separate CLIF
/// legalization pass to run either: legalization happens inside
/// `isa.compile_function`, on the way to vcode, and never touches this text.
///
/// So the CLIF dumped here is **what the builder produced, tidied**: dead blocks
/// and trivial block parameters gone, value aliases collapsed. That is worth
/// knowing in both directions — a block or a `jump` argument the lowering
/// emitted may be missing here, and nothing else will be.
pub(crate) fn emit(name: &str, ctx: &codegen::Context) {
    let hooks = hooks();
    if !hooks.selects(name) {
        return;
    }
    let compiled = ctx.compiled_code().and_then(|cc| {
        cc.vcode.as_deref().map(|listing| Compiled {
            listing,
            bytes: cc.code_buffer().len(),
        })
    });
    eprint!("{}", render(hooks, name, &ctx.func, compiled));
}

/// What the compiled code tells the machine-level dump: Cranelift's own
/// listing, and the exact size of the code it emitted.
///
/// The byte count is here because it is the one number in the dump that is not
/// a count of *lines*. A listing entry is a machine instruction with a few
/// documented exceptions — `load_ext_name_far` is one entry and a 16-byte
/// literal-pool sequence — so "did this change shrink the code" is answered
/// exactly by the bytes and approximately by the entries, and both belong in the
/// header for a reader who needs to know which they are looking at.
struct Compiled<'a> {
    listing: &'a str,
    bytes: usize,
}

/// The text [`emit`] writes. Split out because it is the whole of the
/// behaviour and a test cannot read the process's stderr.
fn render(
    hooks: &DumpHooks,
    name: &str,
    func: &Function,
    compiled: Option<Compiled<'_>>,
) -> String {
    let mut out = String::new();
    if hooks.clif.covers(name) {
        let counts: Vec<(String, usize)> = func
            .layout
            .blocks()
            .map(|b| (b.to_string(), func.layout.block_insts(b).count()))
            .collect();
        // Every CLIF entry is a block, so the two counts are the same number.
        let blocks = counts.len();
        summarize(&mut out, "clif", name, &counts, blocks, None);
        out.push_str(&func.display().to_string());
    }
    if hooks.vcode.covers(name) {
        match compiled {
            Some(compiled) => {
                let (counts, blocks) = vcode_block_counts(compiled.listing);
                summarize(
                    &mut out,
                    "vcode",
                    name,
                    &counts,
                    blocks,
                    Some(compiled.bytes),
                );
                out.push_str(compiled.listing);
            }
            // Unreachable while `wants_vcode` and `emit` read the same field,
            // which is why this says so rather than panicking: a dump hook that
            // aborts a compile would be worse than the missing dump.
            None => out.push_str(&format!(
                ";; praxis-dump vcode `{name}`: no disassembly was requested before this \
                 function was defined\n"
            )),
        }
    }
    out
}

/// The two header lines: the totals, then the per-block breakdown.
///
/// Both are prefixed `;; praxis-dump` so a dump can be `grep`ped out of a run's
/// stderr, and the per-block line is one line because that is what a package
/// quoting "the loop body went from 216 to 202" pastes.
///
/// **`blocks` is passed in rather than taken as `counts.len()`**, because the
/// vcode breakdown carries one entry — the prologue — that is not a block. Five
/// of handover 26's packages compare a CLIF block count against a vcode block
/// count, and counting the prologue made those two different denominators. When
/// the two disagree the header says `N blocks + prologue`, so a reader has both
/// the comparable number and the entry that explains the extra row below it.
fn summarize(
    out: &mut String,
    what: &str,
    name: &str,
    counts: &[(String, usize)],
    blocks: usize,
    bytes: Option<usize>,
) {
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    let size = bytes.map_or(String::new(), |b| format!(", {b} bytes of machine code"));
    let prologue = if counts.len() > blocks {
        " + prologue"
    } else {
        ""
    };
    out.push_str(&format!(
        ";; praxis-dump {what} `{name}`: {total} instructions in \
         {blocks} blocks{prologue}{size}\n"
    ));
    let per_block: Vec<String> = counts.iter().map(|(b, n)| format!("{b}={n}")).collect();
    out.push_str(&format!(
        ";; praxis-dump {what} `{name}`: {}\n",
        per_block.join(" ")
    ));
}

/// Entries printed into the listing that are directives rather than
/// instructions, so they must not be counted as any.
///
/// `unwind` is the whole of it in practice, and it matters: an aarch64 prologue
/// prints 14 of them against 9 real instructions, so counting them would inflate
/// every prologue by 150%. `dummy_use` and `nop-zero-len` emit nothing at all;
/// `emit_island` asks the buffer for a veneer island rather than emitting an
/// instruction of its own. Named from `Inst::pretty_print_inst`
/// (`isa/aarch64/inst/mod.rs`) against the arms of `emit` that write no bytes.
const PSEUDO_ENTRIES: &[&str] = &["unwind ", "dummy_use ", "nop-zero-len", "emit_island "];

/// Instructions per block in Cranelift's machine-level listing.
///
/// The listing's shape is fixed by `VCode::emit`: a block label at column zero
/// (`block3:`), one entry per line indented by two spaces, and stack maps and
/// debug tags as `  ; …` comments on their own lines. So an instruction is an
/// indented line that is neither a comment nor one of [`PSEUDO_ENTRIES`].
///
/// **One entry is one machine instruction with a few exceptions**, of which
/// `load_ext_name_far` — a 16-byte literal-pool load — is the one this backend
/// emits often. The exact size is in the header line beside this count for
/// exactly that reason.
///
/// The block numbers are the *lowered* block order and are not the CLIF block
/// numbering: a CLIF block can be split, dropped or reordered before it reaches
/// here. The two dumps are comparable in total, not row by row.
///
/// **Returns the breakdown and the block count separately, and they differ.**
/// The instructions before the first label are the function prologue, which
/// belongs to no lowered block: it gets a row in the breakdown, because its
/// instructions are real and are in the total, but it is not counted as a block.
/// It used to be, which made every vcode dump read one block high — `458
/// instructions in 68 blocks` for a function of 67 blocks and a prologue.
fn vcode_block_counts(vcode: &str) -> (Vec<(String, usize)>, usize) {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut blocks = 0;
    for line in vcode.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with(';')
            || PSEUDO_ENTRIES.iter().any(|p| trimmed.starts_with(p))
        {
            continue;
        }
        if !line.starts_with(' ') && trimmed.ends_with(':') {
            counts.push((trimmed.trim_end_matches(':').to_string(), 0));
            blocks += 1;
            continue;
        }
        match counts.last_mut() {
            Some((_, n)) => *n += 1,
            // Before the first label, so `blocks` deliberately does not move.
            None => counts.push(("prologue".to_string(), 1)),
        }
    }
    (counts, blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift::prelude::{types, AbiParam, InstBuilder, Signature};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::Module;

    /// An unset variable and an empty one are the same answer, and it is the
    /// one that costs nothing.
    #[test]
    fn an_unset_or_empty_variable_selects_nothing() {
        assert_eq!(DumpSelection::parse(None), DumpSelection::Nothing);
        assert_eq!(DumpSelection::parse(Some("")), DumpSelection::Nothing);
        assert_eq!(DumpSelection::parse(Some("   ")), DumpSelection::Nothing);
        // A list of nothing but separators is a list that named nothing, and
        // `Named(vec![])` must not be how that is spelled.
        assert_eq!(DumpSelection::parse(Some(",,")), DumpSelection::Nothing);
    }

    /// `1` is the form a shell one-liner types; `all` is the form that reads.
    #[test]
    fn one_and_all_select_every_function() {
        assert_eq!(DumpSelection::parse(Some("1")), DumpSelection::Every);
        assert_eq!(DumpSelection::parse(Some("all")), DumpSelection::Every);
        assert_eq!(DumpSelection::parse(Some("ALL")), DumpSelection::Every);
        assert!(DumpSelection::parse(Some("1")).covers("<entry>"));
    }

    /// Anything else is a comma-separated list of function names, which is how
    /// a measurement package asks about one loop in a real program.
    #[test]
    fn a_value_that_is_not_one_or_all_names_functions() {
        let sel = DumpSelection::parse(Some("<entry>, helper"));
        assert_eq!(
            sel,
            DumpSelection::Named(vec!["<entry>".to_string(), "helper".to_string()])
        );
        assert!(sel.covers("<entry>"));
        assert!(sel.covers("helper"));
        assert!(!sel.covers("main"), "a name not on the list is not covered");
        assert!(
            !sel.covers("<entry>x"),
            "and the match is the whole name, not a prefix"
        );
    }

    /// A scratch Cranelift function: one block, one `iadd`, one return.
    fn scratch_function() -> Function {
        let module = JITModule::new(
            JITBuilder::with_flags(
                crate::module::CRANELIFT_FLAGS,
                cranelift_module::default_libcall_names(),
            )
            .expect("host target is supported"),
        );
        let mut sig = Signature::new(module.isa().default_call_conv());
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(Default::default(), sig);
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let arg = builder.block_params(entry)[0];
        let doubled = builder.ins().iadd(arg, arg);
        builder.ins().return_(&[doubled]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());
        func
    }

    /// Nothing selected is nothing written. This is the property the whole
    /// hook rests on: a benchmark run with neither variable set must produce
    /// byte-identical output to one compiled without the hook at all.
    #[test]
    fn a_selection_of_nothing_renders_nothing() {
        let hooks = DumpHooks {
            clif: DumpSelection::Nothing,
            vcode: DumpSelection::Nothing,
        };
        let compiled = Compiled {
            listing: "block0:\n  ret\n",
            bytes: 4,
        };
        assert_eq!(render(&hooks, "f", &scratch_function(), Some(compiled)), "");
    }

    /// The CLIF dump is the function's own text, headed by the counts the
    /// measurement packages quote.
    #[test]
    fn an_enabled_clif_hook_renders_the_function_and_its_instruction_counts() {
        let hooks = DumpHooks {
            clif: DumpSelection::Every,
            vcode: DumpSelection::Nothing,
        };
        let out = render(&hooks, "f", &scratch_function(), None);
        assert!(
            out.contains(";; praxis-dump clif `f`: 2 instructions in 1 blocks"),
            "an iadd and a return, in one block:\n{out}"
        );
        assert!(
            out.contains(";; praxis-dump clif `f`: block0=2"),
            "and the per-block breakdown is one line:\n{out}"
        );
        assert!(out.contains("iadd"), "followed by the IR itself:\n{out}");
        assert!(
            !out.contains("praxis-dump vcode"),
            "and the vcode hook was not asked for:\n{out}"
        );
    }

    /// A function the hook does not name is not dumped, which is the whole
    /// point of naming one.
    #[test]
    fn a_function_the_hook_does_not_name_renders_nothing() {
        let hooks = DumpHooks {
            clif: DumpSelection::Named(vec!["other".to_string()]),
            vcode: DumpSelection::Nothing,
        };
        assert_eq!(render(&hooks, "f", &scratch_function(), None), "");
    }

    /// The vcode dump is Cranelift's own disassembly, counted the same way.
    #[test]
    fn an_enabled_vcode_hook_renders_the_disassembly_and_its_counts() {
        let hooks = DumpHooks {
            clif: DumpSelection::Nothing,
            vcode: DumpSelection::Every,
        };
        let listing = concat!(
            "  pacibsp\n",
            "  unwind Aarch64SetPointerAuth { return_addresses: true }\n",
            "block0:\n",
            "  stp x29, x30, [sp, #-16]!\n",
            "  ; UserStackMap { … }\n",
            "  add x0, x0, x0\n",
            "block1:\n",
            "  ret\n",
        );
        let out = render(
            &hooks,
            "f",
            &scratch_function(),
            Some(Compiled { listing, bytes: 16 }),
        );
        assert!(
            out.contains(
                ";; praxis-dump vcode `f`: 4 instructions in 2 blocks + prologue, 16 bytes"
            ),
            "neither the stack-map comment nor the unwind directive is an \
             instruction; the byte count is the exact one; and the prologue is \
             beside the block count rather than inside it, because the packages \
             that compare this against a CLIF block count need one \
             denominator:\n{out}"
        );
        assert!(
            out.contains(";; praxis-dump vcode `f`: prologue=1 block0=2 block1=1"),
            "the instructions before the first label still get their own row — \
             they are real instructions and they are in the total of 4:\n{out}"
        );
        assert!(out.contains("stp x29"), "followed by the listing:\n{out}");
    }

    /// A function whose listing starts at a label has no prologue row, and then
    /// the header says plain `N blocks` with nothing to qualify.
    #[test]
    fn a_vcode_listing_with_no_prologue_counts_only_blocks() {
        let hooks = DumpHooks {
            clif: DumpSelection::Nothing,
            vcode: DumpSelection::Every,
        };
        let listing = concat!("block0:\n", "  add x0, x0, x0\n", "  ret\n");
        let out = render(
            &hooks,
            "f",
            &scratch_function(),
            Some(Compiled { listing, bytes: 8 }),
        );
        assert!(
            out.contains(";; praxis-dump vcode `f`: 2 instructions in 1 blocks, 8 bytes"),
            "one block, and no `+ prologue` to explain away:\n{out}"
        );
        assert!(
            !out.contains("prologue"),
            "and no prologue row either:\n{out}"
        );
    }

    /// If the disassembly is missing the dump says so rather than panicking:
    /// a hook that aborts a compile is worse than a hook that prints nothing.
    #[test]
    fn a_vcode_hook_with_no_disassembly_says_so_instead_of_panicking() {
        let hooks = DumpHooks {
            clif: DumpSelection::Nothing,
            vcode: DumpSelection::Every,
        };
        let out = render(&hooks, "f", &scratch_function(), None);
        assert!(out.contains("no disassembly was requested"), "{out}");
    }
}
