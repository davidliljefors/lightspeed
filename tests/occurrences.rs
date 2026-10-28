//! The resolved-occurrence index may miss a use, but must never record a wrong one.

mod common;

use common::Ws;
use lightspeed::query::{DiskText, Query};

/// What is stored for the identifier at `needle` (an exact substring, unique in the file).
fn stored(ws: &Ws, tus: &[&str], file: &str, needle: &str) -> Vec<String> {
    let idx = ws.index(tus);
    let q = Query::new(&idx, &DiskText);
    let path = ws.path(file);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches(needle).count(), 1, "{needle} must be unique");
    let at = text.find(needle).unwrap() as u32;
    let (occ, _) = q.build_occurrences();
    let fi = idx.live_files().find(|(_, f)| f.path == path).map(|(i, _)| i).expect("file indexed");
    occ[fi]
        .sites
        .iter()
        .filter(|(o, _)| *o >= at && (*o as usize) < at as usize + needle.len())
        .map(|&(_, s)| format!("{}@{}", idx.db.qualified_name(s), idx.db.file_path(s).rsplit('/').next().unwrap()))
        .collect()
}

#[test]
fn sole_candidate_the_file_cannot_see_is_not_stored() {
    // `Handle` here is a system type; the only indexed `Handle` belongs to an unrelated library.
    let ws = Ws::new(&[("lib.h", "typedef int Handle;\n"), ("b.cpp", "#include \"lib.h\"\n"), ("a.cpp", "void f() {\n  Handle h;\n}\n")], &[]);
    assert!(stored(&ws, &["a.cpp", "b.cpp"], "a.cpp", "Handle").is_empty());
}

#[test]
fn header_relying_on_its_includer_is_not_pinned_to_one_library() {
    let ws = Ws::new(
        &[
            ("liba.h", "struct Widget { int a; };\n"),
            ("libb.h", "struct Widget { int b; };\n"),
            ("inner.h", "inline int use(Widget* w) { return 0; }\n"),
            ("a.cpp", "#include \"liba.h\"\n#include \"inner.h\"\n"),
            ("b.cpp", "#include \"libb.h\"\n#include \"inner.h\"\n"),
        ],
        &[],
    );
    assert!(stored(&ws, &["a.cpp", "b.cpp"], "inner.h", "Widget").is_empty());
}

#[test]
fn header_seen_under_two_libraries_is_not_pinned_to_one() {
    let ws = Ws::new(
        &[
            ("liba.h", "struct Widget { int a; };\n"),
            ("libb.h", "struct Widget { int b; };\n"),
            ("inner.h", "#include \"liba.h\"\ninline int use(Widget* w) { return 0; }\n"),
            ("a.cpp", "#include \"inner.h\"\n"),
            ("b.cpp", "#include \"libb.h\"\n#include \"inner.h\"\n"),
        ],
        &[],
    );
    assert!(stored(&ws, &["a.cpp", "b.cpp"], "inner.h", "Widget").is_empty());
}

#[test]
fn unambiguous_use_is_stored() {
    let ws = Ws::new(&[("lib.h", "struct Widget { int a; };\n"), ("a.cpp", "#include \"lib.h\"\nWidget w;\n")], &[]);
    assert_eq!(stored(&ws, &["a.cpp"], "a.cpp", "Widget"), ["Widget@lib.h"]);
}

#[test]
fn macro_in_effect_is_not_replaced_by_a_class_of_the_same_name() {
    let ws = Ws::new(
        &[
            ("m.h", "#define Thing int\n"),
            ("c.h", "struct Thing {};\n"),
            ("a.cpp", "#include \"m.h\"\n#include \"c.h\"\nvoid f() {\n  Thing t;\n}\n"),
        ],
        &[],
    );
    let got = stored(&ws, &["a.cpp"], "a.cpp", "Thing t");
    assert!(!got.iter().any(|s| s.ends_with("c.h")), "{got:?}");
}

#[test]
fn alias_of_nothing_is_not_merged_with_a_struct_of_the_same_name() {
    let ws = Ws::new(
        &[
            ("s.h", "struct Foo { int x; };\n"),
            ("t.h", "typedef int Foo;\n"),
            ("a.cpp", "#include \"s.h\"\n#include \"t.h\"\nvoid f() {\n  Foo v;\n}\n"),
        ],
        &[],
    );
    assert!(stored(&ws, &["a.cpp"], "a.cpp", "Foo v").is_empty());
}

#[test]
fn same_named_macros_from_different_headers_are_not_merged() {
    let ws = Ws::new(
        &[
            ("a.h", "#define LIMIT 1\n"),
            ("b.h", "#undef LIMIT\n#define LIMIT 2\n"),
            ("a.cpp", "#include \"a.h\"\n#include \"b.h\"\nint v = LIMIT;\n"),
        ],
        &[],
    );
    assert!(stored(&ws, &["a.cpp"], "a.cpp", "LIMIT;").is_empty());
}

#[test]
fn alias_qualifier_is_not_shadowed_by_a_class_elsewhere() {
    let ws = Ws::new(
        &[
            ("real.h", "struct Real { static int get(); };\n"),
            ("other.h", "namespace lib { struct RD { static int get(); }; }\n"),
            ("a.cpp", "#include \"real.h\"\n#include \"other.h\"\nusing RD = Real;\nint v = RD::get();\n"),
        ],
        &[],
    );
    let got = stored(&ws, &["a.cpp"], "a.cpp", "get();");
    assert!(got.iter().all(|s| s.starts_with("Real::get")), "{got:?}");
}
