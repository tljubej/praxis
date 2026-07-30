//! Diagnostic construction for name resolution and (later) type inference.
//!
//! Name-resolution diagnostics use the `N0xx` prefix; type diagnostics use
//! `Y0xx`. Both reuse the existing [`Diagnostic`] type from `praxis-source` —
//! there is no separate error channel (§8.2, rule 20.6).
//!
//! The `Y0xx` constructors are used by type inference (Slice 5); they are kept
//! here alongside the `N0xx` ones so all diagnostic wording lives in one place.

#![allow(dead_code)] // Y0xx constructors are exercised in Slice 5.

use praxis_source::{DiagCode, Diagnostic, FileSpan, Severity};

/// `N001` — a name was used that is not in scope.
pub(crate) fn unresolved_name(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnknownName,
        format!("`{name}` is not defined"),
        at,
    )
}

/// `N002` — a type annotation names a type that is not a known built-in. This
/// covers reserved-but-unimplemented scalars (`Float`, `UInt`, …, per §4.3) and
/// plain typos.
pub(crate) fn unknown_type(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnknownType,
        format!("unknown type `{name}`"),
        at,
    )
}

/// `N003` — a name in type position resolves to a value, not a type (TY-11).
///
/// Annotation validation asked only whether the name resolved *at all*, so
/// `let Alias = 1` followed by `let value: Alias = "text"` was accepted:
/// `Alias` was in scope, the annotation named no type, and inference quietly
/// used a fresh variable. `N002` would be the wrong report — the name is
/// known; it is the wrong sort of thing.
pub(crate) fn name_is_not_a_type(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NameIsNotAType,
        format!("`{name}` is a value, not a type"),
        at,
    )
}

/// `N004` — a name is declared twice in one scope (TY-24).
///
/// Two top-level `fn`s of one name used to bind two distinct symbols in the
/// same scope: the second overwrote the first in the scope map while both kept
/// their `decls` entry, so both reached the backend and were emitted under one
/// JIT symbol name.
pub(crate) fn duplicate_declaration(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::DuplicateDeclaration,
        format!("`{name}` is already declared in this scope"),
        at,
    )
}

/// `N005` — a function is declared inside a function (TY-23).
///
/// The grammar parses one and name resolution never declared it, so inference
/// reached an `expect` on the missing declaration and panicked — which broke
/// `analyze`'s contract that malformed input becomes diagnostics.
pub(crate) fn nested_function(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NestedFunction,
        format!("`{name}` cannot be declared inside another function"),
        at,
    )
}

/// `N005` — a `struct`/`enum` declared inside a function body (REP-06).
///
/// The same code as a nested `fn`, because it is the same mistake: only the
/// source file's own statements are a declaration position. The wording differs
/// by one word — "another function" is right when the nested thing is itself a
/// function and wrong when it is a type.
pub(crate) fn nested_declaration(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NestedFunction,
        format!("`{name}` cannot be declared inside a function"),
        at,
    )
}

/// `N007` — a `fn` body naming a binding declared outside it (REP-22, ADR-068).
///
/// ```praxis
/// let x = 1
/// fn f() { x }        // N007
/// ```
///
/// It resolved, silently, and answered **`Unit`** — the binding is a local of
/// whatever function encloses it (the file's own generated entry, after
/// ADR-067), and a `fn` body has no slot for another function's local. Through a
/// closure it was worse: `fn g() { |n| n + x }` captured a symbol with no slot,
/// so the environment cell held whatever the read found and `g()(1)` printed a
/// nine-digit number.
///
/// A `fn` does not capture — §4.9 describes functions and §4.10 describes
/// closures, and only the second says "capture". So the message names the
/// distinction and the two ways out, because both are ordinary: pass it as a
/// parameter, or write a closure.
pub(crate) fn function_reads_outer_binding(at: FileSpan, name: &str, func: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::FunctionReadsOuterBinding,
        format!(
            "`{func}` cannot use `{name}`: a function does not capture the bindings around it \
             (pass `{name}` as a parameter, or use a closure)"
        ),
        at,
    )
}

/// `N006` — a `struct`/`enum` declaration that refers to itself (REP-14,
/// ADR-063).
///
/// The declaration pass registers types in dependency order; a declaration in a
/// cycle never becomes ready, and the recursive member used to fall back to a
/// **fresh type variable** with no report. That is not merely silence: a variable
/// unifies with everything, so `struct Node { next: Node, value: Int }` accepted
/// `Node { next: 7, value: 1 }` and ran it.
///
/// `through` names the other declarations in the cycle when there are any, so a
/// mutual pair says which two. Wording follows §5.4: it says what the program
/// wrote and that it is not supported, and it never says "equirecursive". It does
/// not claim the *values* are impossible — every Praxis field holds a reference,
/// so `struct Node { children: Vec[Node] }` describes a perfectly ordinary tree.
/// What is missing is the language feature (ADR-052), and saying so is honest
/// where "cannot contain itself" would not be.
pub(crate) fn recursive_type_declaration(
    at: FileSpan,
    name: &str,
    through: &[String],
) -> Diagnostic {
    let how = if through.is_empty() {
        format!("`{name}` refers to itself")
    } else {
        format!("`{name}` refers to itself through {}", list_names(through))
    };
    Diagnostic::new(
        Severity::Error,
        DiagCode::RecursiveTypeDeclaration,
        format!("{how}, and a self-referring type is not supported"),
        at,
    )
}

/// `N008` — a record literal whose head does not name a `struct` (REP-26).
///
/// ```praxis
/// let x = 1
/// let p = x { a: 1 }      // N008
/// ```
///
/// Nothing checked, so the literal kept the head's own type and lowered to
/// nothing: `out(p)` printed `Unit` and `out(p + 1)` printed a raw pointer. That
/// is REP-01's shape — a program `praxis check` accepts whose value has no
/// representation — and it is why the report is made in inference rather than at
/// lowering (REP-12).
///
/// A declaration mistake, so it is in the Name category next to `N003` ("a name
/// used in type position that names a value"): a record literal's head *is* a type
/// position, and what is wrong is which declaration the name reaches. The kind is
/// named because that is the whole of the answer — an `enum` is a type and still
/// has no fields to initialize.
pub(crate) fn not_a_record_literal_head(at: FileSpan, name: &str, kind: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotARecordLiteralHead,
        format!("`{name}` is {kind}, so `{name} {{ … }}` does not build a record"),
        at,
    )
}

/// `` `A` ``, `` `A` and `B` ``, `` `A`, `B` and `C` `` — for a message that
/// names a cycle's other members.
fn list_names(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// `Y019` — a `.n` element access on a receiver that is not a tuple (REP-08).
///
/// A tuple has no field *names*, so `Y112`'s "no field `0` on this type" would
/// name the wrong thing. Wording follows §5.4: it says what the program did.
pub(crate) fn not_a_tuple(at: FileSpan, ty: &str, index: usize) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NoTupleElement,
        format!("values of type `{ty}` have no element `{index}` — only a tuple does"),
        at,
    )
}

/// `Y019` — a `.n` past the end of a tuple (REP-08).
///
/// The same code as [`not_a_tuple`] and a different message: the receiver *is* a
/// tuple, so naming its arity is the useful thing to say.
pub(crate) fn tuple_index_out_of_range(at: FileSpan, arity: usize, index: usize) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NoTupleElement,
        format!(
            "a tuple of {arity} elements has no element `{index}` — its elements are \
             `0` to `{}`",
            arity - 1
        ),
        at,
    )
}

/// `Y020` — a subscript **read** on a type that has none (REP-16).
///
/// `indices` is in the message because arity is part of what selects the
/// operation: `Grid[T]` indexes at two and `Map[K, V]` at one, so `grid[x]` is a
/// mistake about a receiver that does index. The signature is
/// `fn(FileSpan, &str, usize)` so it can be passed as the `unresolved` report of
/// [`crate::infer`]'s catalog dispatch.
pub(crate) fn not_indexable(at: FileSpan, ty: &str, indices: usize) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotIndexable,
        format!("values of type `{ty}` cannot be indexed with {indices} index(es)"),
        at,
    )
}

/// `Y020` — a subscript **store** on a type that has none (REP-16).
///
/// The same code as [`not_indexable`] and a different message, because the two
/// halves of the surface are not the same set: a `Vec` reads through `v[0]` and
/// has no element store in the language at all, so "cannot be indexed" would be
/// wrong about it while "cannot be assigned through" is exact.
pub(crate) fn not_index_assignable(at: FileSpan, ty: &str, indices: usize) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotIndexable,
        format!("values of type `{ty}` cannot be assigned through {indices} index(es)"),
        at,
    )
}

/// `Y020` — a `min=` update on a type that has none (REP-21).
///
/// Its own message beside [`not_index_assignable`] because a `Counter` *can* be
/// assigned through one index — `c[k] = n` is a row — and what it has not is the
/// updating store. "Cannot be assigned through 1 index" would be false about the
/// very receiver it is most likely to be written for.
pub(crate) fn not_index_min_updatable(at: FileSpan, ty: &str, indices: usize) -> Diagnostic {
    not_index_updatable(at, ty, indices, "min=")
}

/// `Y020` — a `max=` update on a type that has none (REP-21).
pub(crate) fn not_index_max_updatable(at: FileSpan, ty: &str, indices: usize) -> Diagnostic {
    not_index_updatable(at, ty, indices, "max=")
}

fn not_index_updatable(at: FileSpan, ty: &str, indices: usize, op: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotIndexable,
        format!("values of type `{ty}` cannot be updated with `{op}` through {indices} index(es)"),
        at,
    )
}

/// `Y021` — an assignment whose left side names no storage (REP-16).
pub(crate) fn not_an_assignment_target(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotAnAssignmentTarget,
        "the left side of an assignment must be a name or an index".to_string(),
        at,
    )
}

/// `Y001` — two types that could not be unified (expected vs found).
pub(crate) fn type_mismatch(at: FileSpan, expected: &str, found: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::TypeMismatch,
        format!("expected {expected}, found {found}"),
        at,
    )
}

/// `Y001` with an advisory `help:` hint attached (§8.2: "a concrete suggestion
/// when available"). Use when the fix is explanatory rather than mechanical
/// (e.g. "this value is `Unit`; make the last expression produce a value").
pub(crate) fn type_mismatch_with_help(
    at: FileSpan,
    expected: &str,
    found: &str,
    help_label: &str,
) -> Diagnostic {
    Diagnostic::build(
        Severity::Error,
        DiagCode::TypeMismatch,
        format!("expected {expected}, found {found}"),
        at,
    )
    .help(at, help_label)
    .finish()
}

/// `Y002` — an occurs-check failure (infinite type), e.g. unifying `a` with
/// `(a) -> a`.
pub(crate) fn infinite_type(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::InfiniteType,
        "an infinite type would be required here",
        at,
    )
}

/// `Y003` — an explicit annotation conflicts with what inference derived.
pub(crate) fn annotation_conflict(at: FileSpan, annotated: &str, derived: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::AnnotationConflict,
        format!("annotation says {annotated}, but use implies {derived}"),
        at,
    )
}

/// `Y004` — `==` / `!=` applied to a type whose values cannot be compared (e.g.
/// a function value). The wording is concrete (§5.4: never mention trait or
/// capability names): it says the type "cannot be compared" and names the type.
pub(crate) fn not_equatable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotEquatable,
        format!("values of type `{ty}` cannot be compared with `==`"),
        at,
    )
}

/// `Y005` — a value is iterated (`for x in …`) whose type is not iterable (§4.11,
/// §5.4 `Iterable`). Wording is concrete and never names the capability (§5.4).
pub(crate) fn not_iterable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotIterable,
        format!("values of type `{ty}` cannot be iterated"),
        at,
    )
}

/// `Y006` — a value is used where an orderable type is required (heap element,
/// sort, comparison) but its type is not orderable (§5.4 `SupportsOrd`). Wording
/// is concrete and never names the capability (§5.4).
pub(crate) fn not_orderable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotOrderable,
        format!("values of type `{ty}` cannot be ordered"),
        at,
    )
}

/// `Y014` — a value is used as a `Map` key or a `Set` element but cannot be
/// found again once it changes (D4, TY-32/RT-08).
///
/// The wording is the *reason*, not the rule (§5.4: never name the capability).
/// A key is looked up by its contents; a `Vec` that is pushed to after it is
/// stored hashes to a different bucket than the one holding it, so the entry
/// becomes unreachable. Saying "not hashable" would be both jargon and a lie —
/// a `Vec` hashes fine.
pub(crate) fn not_hashable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::build(
        Severity::Error,
        DiagCode::NotHashable,
        format!(
            "a value of type `{ty}` can change after it is stored, so it cannot be used as a key"
        ),
        at,
    )
    .help(
        at,
        "use a value that cannot change — a number, `Text`, or a tuple of those",
    )
    .finish()
}

/// `Y015` — arithmetic on a type that has none (TY-31).
///
/// Concrete wording again: it names the operation the program wrote and the
/// type it wrote it on, and never says "numeric constraint".
pub(crate) fn not_numeric(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotNumeric,
        format!("values of type `{ty}` cannot be used in arithmetic"),
        at,
    )
}

/// `Y110` at a *use* site rather than at a method name: a generic function's
/// body called a method on a parameter, and this call instantiated that
/// parameter at a type with no such method (TY-30).
pub(crate) fn unknown_method(at: FileSpan, name: &str, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NoMethodOnType,
        format!("no method `{name}` on type `{ty}`"),
        at,
    )
}

/// `Y007` — a type constructor in an annotation was given the wrong number of
/// type arguments, e.g. `Map[Int]` or `Vec[Int, Text]`.
///
/// The arity was declared by [`CollectionCtor::arity`](praxis_types::CollectionCtor::arity)
/// all along and nothing consulted it (TY-07): a wrong-arity annotation
/// interned a type that could never unify with anything, so the user saw a
/// downstream `Y001` naming a type they did not write. This names the mistake
/// where it was made.
pub(crate) fn wrong_type_argument_count(
    at: FileSpan,
    ctor: &str,
    got: usize,
    want: usize,
) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::WrongTypeArgumentCount,
        format!("`{ctor}` takes {want} type argument(s), but {got} were given"),
        at,
    )
}

/// `Y008` — a `struct` or `enum` declaration names the same field or variant
/// twice.
///
/// Also TY-07. The def used to register with both members and every lookup
/// answered the first, so `struct P { x: Int, x: Text }` silently declared a
/// one-field record whose second `x` was unreachable.
pub(crate) fn duplicate_member(at: FileSpan, what: &str, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::DuplicateMember,
        format!("duplicate {what} `{name}`"),
        at,
    )
}

/// `Y120` — a `match` is not exhaustive: some values of the scrutinee type are
/// not covered by any arm. The missing constructors are named to guide the fix.
pub(crate) fn non_exhaustive(at: FileSpan, missing: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NonExhaustiveMatch,
        format!("non-exhaustive match: missing {missing}"),
        at,
    )
}

/// `Y121` — a `match` arm is unreachable: an earlier arm already matched all
/// the values this arm could match.
pub(crate) fn unreachable_arm(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnreachableArm,
        "unreachable match arm",
        at,
    )
}

/// `Y113` — a record literal that does not initialize every declared field
/// (HIR-04).
///
/// A missing field was allocated as `Unit` under the field's declared type, so
/// the object's schema and its payloads disagreed and every later read of that
/// field got a `Unit` the type system said was an `Int`.
pub(crate) fn missing_record_fields(
    at: FileSpan,
    type_name: &str,
    missing: &[String],
) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::MissingRecordFields,
        format!(
            "`{type_name}` literal is missing {}: {}",
            if missing.len() == 1 {
                "a field"
            } else {
                "fields"
            },
            missing.join(", ")
        ),
        at,
    )
}

/// `Y123` — a pattern whose shape cannot match, with the reason spelled out
/// (REP-10).
///
/// The shape is what is wrong, not the type it was matched against: a tuple
/// pattern with one element and a record pattern whose head is not a record both
/// name something no value can be.
pub(crate) fn not_a_pattern(at: FileSpan, reason: &str) -> Diagnostic {
    Diagnostic::new(Severity::Error, DiagCode::NotAPatternForType, reason, at)
}

/// `Y115` — a record *pattern* naming one field twice (REP-10).
///
/// The same code as a literal's duplicate, because it is the same mistake read
/// in the other direction: the second sub-pattern would silently replace the
/// first, so one of the two bindings the program wrote would never happen.
pub(crate) fn duplicate_pattern_field(at: FileSpan, field: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::DuplicateRecordField,
        format!("field `{field}` is matched more than once"),
        at,
    )
}

/// `Y114` — a record literal naming a field the type does not have (HIR-04).
///
/// The initializer was not lowered at all, so its side effects disappeared. A
/// record *pattern* naming one is the same mistake and the same code (REP-10).
pub(crate) fn unknown_record_field(at: FileSpan, type_name: &str, field: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnknownRecordField,
        format!("`{type_name}` has no field `{field}`"),
        at,
    )
}

/// `Y115` — a record literal naming one field twice (HIR-04).
///
/// Both payloads were pushed, so the object had more values than its schema had
/// slots and every field after the duplicate read the wrong one.
pub(crate) fn duplicate_record_field(at: FileSpan, field: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::DuplicateRecordField,
        format!("field `{field}` is initialized more than once"),
        at,
    )
}

/// `Y009` — assignment to a binding that is not a `var` (TY-14).
///
/// Assignment never asked what kind of binding it was writing to, so `let x = 1;
/// x = 2` type-checked and lowering emitted the store. Naming the kind is what
/// makes the fix obvious: the answer is almost always to write `var`.
pub(crate) fn assign_to_immutable(at: FileSpan, name: &str, kind: &str) -> Diagnostic {
    Diagnostic::build(
        Severity::Error,
        DiagCode::AssignToImmutable,
        format!("cannot assign to `{name}`, which is {kind}"),
        at,
    )
    .help(
        at,
        format!("declare it with `var {name}` to allow assignment"),
    )
    .finish()
}

/// `Y010` — a compound assignment whose target is not numeric (TY-15).
///
/// `x += e` is arithmetic, and arithmetic is defined on `Int` and `Float`
/// (§4.12). The check was that the two operand types *matched*, which
/// `var flag = true; flag += false` satisfies. Wording is concrete and never
/// names the capability (§5.4).
pub(crate) fn compound_assign_non_numeric(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::CompoundAssignNonNumeric,
        format!("values of type `{ty}` do not support this operation"),
        at,
    )
}

/// `Y011` — `return` with no function to return from (TY-20).
///
/// The analyzer tracked no function context, so a top-level `return` passed
/// every check and reached MIR, whose builder tolerated the missing context
/// with an `if let`. The mistake has a source position; this is where it is
/// reported.
pub(crate) fn return_outside_function(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::ReturnOutsideFunction,
        "`return` outside a function",
        at,
    )
}

/// `Y012` — `break` or `continue` with no loop to leave (TY-20).
///
/// A closure is a function boundary, so a loop *outside* a closure is not one a
/// `break` inside it can leave — which is why the depth is cleared and restored
/// around a closure body rather than simply counted.
pub(crate) fn outside_loop(at: FileSpan, keyword: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::BreakOutsideLoop,
        format!("`{keyword}` outside a loop"),
        at,
    )
}

/// `Y017` — a `break` carrying a value out of a `while`/`for` (TY-21, D2).
///
/// Only `loop` is an expression loop. A `while` or `for` leaves by its condition
/// failing as well as by a `break`, and there is no value the compiler could
/// supply on that path — so a `break` there cannot carry one. This is not
/// `Y012`: the loop exists, and it is the *kind* of loop that is wrong.
pub(crate) fn value_break_outside_loop_expression(at: FileSpan, keyword: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::ValueBreakOutsideLoopExpression,
        format!("a `break` carrying a value needs a `loop`; a `{keyword}` produces `Unit`"),
        at,
    )
}

/// `Y016` — an operator the language does not define for this operand type
/// (TY-27). Not a *mismatch*: both operands agree, and the operation still has
/// no meaning.
pub(crate) fn operator_not_defined(at: FileSpan, op: &str, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::OperatorNotDefined,
        format!("`{op}` is not defined for `{ty}`"),
        at,
    )
}

/// `Y018` — a **generic** `fn` used as a value (REP-01, ADR-061).
///
/// The wording names the remedy because there is one and it is exact: a closure
/// body *is* a call site, so `|x| id(x)` gives monomorphization the
/// instantiation a bare value cannot carry. Saying "monomorphization" would name
/// the machinery instead of the fix.
pub(crate) fn generic_function_as_value(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::GenericFunctionAsValue,
        format!(
            "`{name}` is generic, so it has no single function value; \
             write `|x| {name}(x)` to fix its type arguments at the call"
        ),
        at,
    )
}
