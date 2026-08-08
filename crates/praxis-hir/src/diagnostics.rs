//! Diagnostic construction for name resolution and type inference.
//!
//! Name-resolution diagnostics use the `N0xx` prefix; type diagnostics use
//! `Y0xx`. Both reuse the existing [`Diagnostic`] type from `praxis-source` —
//! there is no separate error channel (§8.2, rule 20.6). The `Y0xx`
//! constructors are used by type inference and kept here alongside the `N0xx`
//! ones so all diagnostic wording lives in one place.
//!
//! There is deliberately no blanket `allow(dead_code)` here. Every constructor
//! is either called directly or handed to [`crate::infer`]'s catalog dispatch
//! as a function pointer — which the lint already counts as a use. A
//! constructor with no emitter is a diagnostic the compiler cannot report; let
//! the lint say so.

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

/// `N003` — a name in type position resolves to a value, not a type.
///
/// Annotation validation asks more than whether the name resolves *at all*:
/// `var Alias = 1` followed by `var value: Alias = "text"` has `Alias` in scope
/// while the annotation names no type. `N002` would be the wrong report — the
/// name is known; it is the wrong sort of thing.
pub(crate) fn name_is_not_a_type(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NameIsNotAType,
        format!("`{name}` is a value, not a type"),
        at,
    )
}

/// `N004` — a name is declared twice in one scope.
///
/// Two top-level `fn`s of one name would otherwise bind two distinct symbols in
/// the same scope and reach the backend under one JIT symbol name.
pub(crate) fn duplicate_declaration(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::DuplicateDeclaration,
        format!("`{name}` is already declared in this scope"),
        at,
    )
}

/// `N005` — a function is declared inside a function.
///
/// The grammar parses one and name resolution does not declare it, so the
/// report has to be made here: `analyze`'s contract is that malformed input
/// becomes diagnostics rather than a panic in inference.
pub(crate) fn nested_function(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NestedFunction,
        format!("`{name}` cannot be declared inside another function"),
        at,
    )
}

/// `N005` — a `struct`/`enum` declared inside a function body.
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

/// `N007` — a `fn` body naming a binding declared outside it (ADR-068).
///
/// ```praxis
/// var x = 1
/// fn f() { x }        // N007
/// ```
///
/// The binding is a local of whatever function encloses it (the file's own
/// generated entry, ADR-067), and a `fn` body has no slot for another
/// function's local: unreported, the read answers `Unit`, and through a closure
/// — `fn g() { |n| n + x }` — the captured symbol has no slot at all.
///
/// A `fn` does not capture — §4.9 describes functions and §4.10 describes
/// closures, and only the second says "capture". So the message names the
/// distinction and the two ways out, because both are ordinary: pass it as a
/// parameter, or write a closure.
///
/// The closure half is conditional. A closure cannot name itself — a `var`'s
/// initializer is resolved in the *preceding* environment, so
/// `var f = |n| … f(n - 1) …` is `N001` — which means that for a **recursive**
/// `fn`, exactly the case where threading state through the parameter list
/// hurts, "or use a closure" is advice the compiler itself refuses. So
/// `recursive_through` drops that clause and attaches a `help:` line saying why,
/// naming the other members of the call cycle the way `N006` names the other
/// members of a type cycle. `None` — the common, non-recursive case — carries
/// the plain message.
pub(crate) fn function_reads_outer_binding(
    at: FileSpan,
    name: &str,
    func: &str,
    recursive_through: Option<&[String]>,
) -> Diagnostic {
    let Some(through) = recursive_through else {
        return Diagnostic::new(
            Severity::Error,
            DiagCode::FunctionReadsOuterBinding,
            format!(
                "`{func}` cannot use `{name}`: a function does not capture the bindings around it \
                 (pass `{name}` as a parameter, or use a closure)"
            ),
            at,
        );
    };
    let how = if through.is_empty() {
        format!("`{func}` calls itself")
    } else {
        format!("`{func}` calls itself through {}", list_names(through))
    };
    Diagnostic::build(
        Severity::Error,
        DiagCode::FunctionReadsOuterBinding,
        format!(
            "`{func}` cannot use `{name}`: a function does not capture the bindings around it \
             (pass `{name}` as a parameter)"
        ),
        at,
    )
    .help(
        at,
        format!("{how}, so a closure is not the way out: a closure cannot name itself (`N001`)"),
    )
    .finish()
}

/// `N006` — a `struct`/`enum` declaration that refers to itself (ADR-063).
///
/// The declaration pass registers types in dependency order, so a declaration
/// in a cycle never becomes ready. Reporting it is what keeps the recursive
/// member from falling back to a **fresh type variable**, which unifies with
/// everything: `struct Node { next: Node, value: Int }` would then accept
/// `Node { next: 7, value: 1 }`.
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

/// `N008` — a record literal whose head does not name a `struct`.
///
/// ```praxis
/// var x = 1
/// var p = x { a: 1 }      // N008
/// ```
///
/// Unchecked, the literal keeps the head's own type and lowers to nothing — a
/// program `praxis check` accepts whose value has no representation. That is
/// why the report is made in inference rather than at lowering.
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

/// `Y019` — a `.n` element access on a receiver that is not a tuple.
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

/// `Y019` — a `.n` past the end of a tuple.
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

/// `Y020` — a subscript **read** on a type that has none.
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

/// `Y020` — a subscript **store** on a type that has none.
///
/// The same code as [`not_indexable`] and a different message, because the two
/// halves of the surface are not the same set: a `Text` reads through `t[0]` and
/// is immutable (§4.3), so "cannot be indexed" would be wrong about it while
/// "cannot be assigned through" is exact. `Text` is the whole of what this
/// covers — `Vec` and `Deque` have element stores (ADR-124).
pub(crate) fn not_index_assignable(at: FileSpan, ty: &str, indices: usize) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotIndexable,
        format!("values of type `{ty}` cannot be assigned through {indices} index(es)"),
        at,
    )
}

/// `Y020` — a `min=` update on a type that has none.
///
/// Its own message beside [`not_index_assignable`] because a `Counter` *can* be
/// assigned through one index — `c[k] = n` is a row — and what it has not is the
/// updating store. "Cannot be assigned through 1 index" would be false about the
/// very receiver it is most likely to be written for.
pub(crate) fn not_index_min_updatable(at: FileSpan, ty: &str, indices: usize) -> Diagnostic {
    not_index_updatable(at, ty, indices, "min=")
}

/// `Y020` — a `max=` update on a type that has none.
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

/// `Y023` — a backtick parser template written where a value is expected
/// (ADR-084).
pub(crate) fn parser_template_outside_read(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::ParserTemplateOutsideRead,
        "a backtick template is a parser expression; write `read` before it, or \
         pass it to `parse(text, ...)`"
            .to_string(),
        at,
    )
}

/// `Y021` — an assignment whose left side names no storage.
pub(crate) fn not_an_assignment_target(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotAnAssignmentTarget,
        "the left side of an assignment must be a name, a field, or an index".to_string(),
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

/// `Y024` — a call whose argument count does not match the function's
/// (ADR-089).
///
/// The counts, not the two function types: for `assert(cond, "why")`,
/// *expected `(Bool) -> Unit`, found `(Bool, Text) -> ?T`* is a whole-type diff
/// that reads like an inference accident, where the mistake is arithmetic.
///
/// There is no `assert`-specific wording, and no `help:` naming another
/// signature: ADR-089 decides that a name has exactly one, so there is no other
/// one to point at.
pub(crate) fn arity_mismatch(at: FileSpan, expected: usize, found: usize) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::CallArityMismatch,
        format!("this function takes {expected} argument(s), but {found} were given"),
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
/// found again once it changes.
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

/// `Y015` — arithmetic on a type that has none.
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

/// `Y110` — **the** builder for a method call that cannot resolve (ADR-093).
///
/// The one builder, and it reports from inference rather than from lowering:
/// `praxis check` never runs lowering, so a `Y110` raised there would only
/// arrive at `run`, after a silent `check`. The message names both the type —
/// what §5.4 asks for (concrete language, never the capability) — and the
/// arity, which is what distinguishes `v.get(0)` from `v.get()`.
///
/// `ty` is `None` for the one shape where there is no receiver type to name:
/// nothing has pinned the receiver, and the call is refused anyway because the
/// catalog holds that name at that arity on **no** receiver. Rendering the
/// receiver there would print `?T` — a type variable's leaked name — into a
/// message §5.4 requires to be concrete, and it would be the least useful half
/// of the sentence. The name and the arity are the whole answer.
pub(crate) fn unknown_method(
    at: FileSpan,
    name: &str,
    ty: Option<&str>,
    arity: usize,
) -> Diagnostic {
    let message = match ty {
        Some(ty) => format!("no method `{name}` on type `{ty}` taking {arity} argument(s)"),
        None => format!("no type has a method `{name}` taking {arity} argument(s)"),
    };
    Diagnostic::new(Severity::Error, DiagCode::NoMethodOnType, message, at)
}

/// `Y112` at a *use* site rather than at a field name — [`unknown_method`]'s
/// counterpart for `Capability::HasField`.
///
/// The honest translation of the capability, and the match arm that names it is
/// what keeps it honest if a second emitter appears. `require_field` defers only a
/// receiver that is still a variable, and a deferred one is resolved rather than
/// vetoed, so nothing reaches this today.
pub(crate) fn unknown_field(at: FileSpan, name: &str, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NoFieldOnType,
        format!("no field `{name}` on type `{ty}`"),
        at,
    )
}

/// `Y007` — a type constructor in an annotation was given the wrong number of
/// type arguments, e.g. `Map[Int]` or `Vec[Int, Text]`.
///
/// The arity is declared by
/// [`CollectionCtor::arity`](praxis_types::CollectionCtor::arity) and checked
/// against it here. Unchecked, a wrong-arity annotation interns a type that can
/// never unify with anything, and the user sees a downstream `Y001` naming a
/// type they did not write; this names the mistake where it was made.
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
/// Unreported, the def registers with both members while every lookup answers
/// the first, so `struct P { x: Int, x: Text }` declares a one-field record
/// whose second `x` is unreachable.
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

/// `Y113` — a record literal that does not initialize every declared field.
///
/// A missing field would be allocated as `Unit` under the field's declared
/// type, so the object's schema and its payloads would disagree and every later
/// read of that field would get a `Unit` the type system called an `Int`.
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

/// `Y013` — an integer literal outside the range of `Int`.
///
/// An `Int` is signed 64-bit (§4.3). Saturating the literal to `i64::MAX`
/// instead would not fault later: the program would simply run with a number
/// nobody wrote.
///
/// Two positions raise it, an expression and a literal pattern, and they share
/// this wording because they are one mistake read at two places.
pub(crate) fn int_literal_out_of_range(at: FileSpan, text: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::IntLiteralOutOfRange,
        format!("`{text}` is outside the range of `Int`"),
        at,
    )
}

/// `Y123` — a pattern whose shape cannot match, with the reason spelled out.
///
/// The shape is what is wrong, not the type it was matched against: a tuple
/// pattern with one element and a record pattern whose head is not a record both
/// name something no value can be.
pub(crate) fn not_a_pattern(at: FileSpan, reason: &str) -> Diagnostic {
    Diagnostic::new(Severity::Error, DiagCode::NotAPatternForType, reason, at)
}

/// `Y122` — a variant pattern naming a variant the scrutinee's enum does not
/// have (ADR-091 Decision 5).
///
/// The same code lowering reports, because it is the same mistake seen one
/// phase earlier. Were it raised only in lowering — which runs only on a
/// program analysis accepted — `praxis check` would be clean on a misspelled
/// variant while `praxis run` exited 1 on the same file, and `praxis check` is
/// the command that is supposed to know.
pub(crate) fn unknown_enum_variant(at: FileSpan, type_name: &str, variant: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnknownEnumVariant,
        format!("`{type_name}` has no variant `{variant}`"),
        at,
    )
}

/// `Y115` — a record *pattern* naming one field twice.
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

/// `Y114` — a record literal naming a field the type does not have.
///
/// An unknown field's initializer is not lowered at all, so unreported its side
/// effects would simply disappear. A record *pattern* naming one is the same
/// mistake and the same code.
pub(crate) fn unknown_record_field(at: FileSpan, type_name: &str, field: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnknownRecordField,
        format!("`{type_name}` has no field `{field}`"),
        at,
    )
}

/// `Y115` — a record literal naming one field twice.
///
/// Both payloads are pushed, so unreported the object would carry more values
/// than its schema has slots, and every field after the duplicate would read
/// the wrong one.
pub(crate) fn duplicate_record_field(at: FileSpan, field: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::DuplicateRecordField,
        format!("field `{field}` is initialized more than once"),
        at,
    )
}

/// `Y010` — a compound assignment whose target is not numeric.
///
/// `x += e` is arithmetic, and arithmetic is defined on `Int` and `Float`
/// (§4.12). Checking only that the two operand types *match* would accept
/// `var flag = true; flag += false`. Wording is concrete and never names the
/// capability (§5.4).
pub(crate) fn compound_assign_non_numeric(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::CompoundAssignNonNumeric,
        format!("values of type `{ty}` do not support this operation"),
        at,
    )
}

/// `Y011` — `return` with no function to return from.
///
/// The analyzer tracks the enclosing function context so a top-level `return`
/// is caught here rather than reaching MIR. The mistake has a source position;
/// this is where it is reported.
pub(crate) fn return_outside_function(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::ReturnOutsideFunction,
        "`return` outside a function",
        at,
    )
}

/// `Y012` — `break` or `continue` with no loop to leave.
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

/// `Y017` — a `break` carrying a value out of a `while`/`for`.
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

/// `Y016` — an operator the language does not define for this operand type.
/// Not a *mismatch*: both operands agree, and the operation still has no
/// meaning.
pub(crate) fn operator_not_defined(at: FileSpan, op: &str, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::OperatorNotDefined,
        format!("`{op}` is not defined for `{ty}`"),
        at,
    )
}

/// `Y018` — a **generic** `fn` used as a value (ADR-061).
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

/// `Y022` — a builtin or a constructor named without being called.
///
/// [`generic_function_as_value`]'s neighbour, and its wording follows the same
/// rule: name the remedy, because there is one and it is exact. A nullary name
/// wants its parentheses — which is the whole of `out(pi)` — and one that takes
/// arguments wants the closure, for the reason `Y018` gives.
///
/// `what` says which kind it is, because the two read differently to whoever
/// wrote them: a builtin is a name from the prelude, a constructor is one the
/// program's own `enum` declared.
pub(crate) fn name_has_no_function_value(
    at: FileSpan,
    name: &str,
    what: &str,
    arity: usize,
) -> Diagnostic {
    let remedy = if arity == 0 {
        format!("call it: `{name}()`")
    } else {
        format!("write `|x| {name}(x)` to call it")
    };
    Diagnostic::new(
        Severity::Error,
        DiagCode::NameHasNoFunctionValue,
        format!("`{name}` is {what}, so it has no function value; {remedy}"),
        at,
    )
}
