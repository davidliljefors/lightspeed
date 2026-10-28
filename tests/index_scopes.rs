//! Go-to-definition namespace resolution: qualified types, inner-scope
//! preference, using-directives and a leading `::`.

mod common;

use common::{Ws, cursor, names};
use lightspeed::query::{DiskText, Query};

/// Resolves the `$` marker in `src` (indexed as `a.cpp`) and returns the qualified names.
fn resolve(src: &str) -> Vec<String> {
    let (text, off) = cursor(src);
    let ws = Ws::new(&[("a.cpp", &text)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let defs = q.definition(&ws.path("a.cpp"), &text, off);
    names(&idx, &defs)
}

#[test]
fn qualified_type_in_parameter() {
    let src = "namespace N { struct C {}; }\nnamespace M { struct C {}; }\nvoid f(N::$C& c);\n";
    assert_eq!(resolve(src), ["N::C"]);
}

#[test]
fn qualified_type_in_member_declaration() {
    let src = "namespace N { struct C {}; }\nnamespace M { struct C {}; }\nstruct S { M::$C* p; };\n";
    assert_eq!(resolve(src), ["M::C"]);
}

#[test]
fn qualified_type_in_local_declaration() {
    let src = "namespace N { struct C {}; }\nnamespace M { struct C {}; }\nvoid f() { N::$C c; }\n";
    assert_eq!(resolve(src), ["N::C"]);
}

#[test]
fn qualifier_segment_resolves_namespace() {
    let src = "namespace N { struct C {}; }\nvoid f($N::C& c);\n";
    assert_eq!(resolve(src), ["N"]);
}

#[test]
fn inner_namespace_type_beats_global() {
    let src = "struct X {};\nnamespace A { struct X {}; void f($X* x); }\n";
    assert_eq!(resolve(src), ["A::X"]);
}

#[test]
fn inner_namespace_function_beats_global() {
    let src = "void g();\nnamespace A { void g(); void f() { $g(); } }\n";
    assert_eq!(resolve(src), ["A::g"]);
}

#[test]
fn nested_namespace_prefers_nearest_enclosing() {
    let src = "struct X {};\nnamespace A { struct X {}; namespace B { void f($X* x); } }\n";
    assert_eq!(resolve(src), ["A::X"]);
}

#[test]
fn using_directive_selects_namespace() {
    let src = "namespace A { void g(); }\nnamespace B { void g(); }\nvoid h() { using namespace A; $g(); }\n";
    assert_eq!(resolve(src), ["A::g"]);
}

#[test]
fn file_scope_using_directive_selects_namespace() {
    let src = "namespace A { struct T {}; }\nnamespace B { struct T {}; }\nusing namespace B;\nvoid h($T* t);\n";
    assert_eq!(resolve(src), ["B::T"]);
}

#[test]
fn using_directive_does_not_leak_into_other_function() {
    let src = "namespace A { void g(); }\nnamespace B { void g(); }\nvoid h() { using namespace A; g(); }\nvoid k() { using namespace B; $g(); }\n";
    assert_eq!(resolve(src), ["B::g"]);
}

#[test]
fn leading_colons_select_global() {
    let src = "void g();\nnamespace A { void g(); void f() { ::$g(); } }\n";
    assert_eq!(resolve(src), ["g"]);
}

#[test]
fn leading_colons_qualified_type() {
    let src = "namespace N { struct C {}; }\nnamespace A { namespace N { struct C {}; } void f(::N::$C* c); }\n";
    assert_eq!(resolve(src), ["N::C"]);
}
