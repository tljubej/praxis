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

/// `Y114` — a record literal naming a field the type does not have (HIR-04).
///
/// The initializer was not lowered at all, so its side effects disappeared.
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
