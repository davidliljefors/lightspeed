//! Index build and incremental updates: edits patch the symbol tables and hierarchy in place.

mod common;

use common::{Ws, names};
use lightspeed::base::ids::Idx;
use lightspeed::symbols::SymbolId;
use lightspeed::workspace::index::Index;

fn observable(idx: &Index) -> Vec<String> {
    common::observable(idx, 1)
}

#[test]
fn edits_to_files_without_classes_patch_indexes_and_hierarchy_incrementally() {
    let pad: String = (0..300).map(|i| format!("int pad{i};\n")).collect();
    let h = format!("#pragma once\n{pad}struct Foo {{ virtual void f(); void g(int); void run(); int h(); }};\nstruct Bar : Foo {{ void f() override; }};\n");
    let h = h.as_str();
    let a = |body: &str| format!("#include \"h.h\"\n{body}");
    let ws = Ws::new(&[("h.h", h), ("a.cpp", &a("void Foo::f() {}\nvoid Foo::g(int a) {}\nvoid Foo::run() {}\nvoid free1() {}\n"))], &[]);
    let mut idx = ws.index(&["a.cpp"]);
    let p = ws.path("a.cpp");
    let edits = [
        "void Foo::f() {}\nvoid Foo::g(int a) {}\nvoid Foo::ru\nn() {}\nvoid free1() {}\n",
        "void Foo::f() {}\nvoid Foo::g(float a) {}\nvoid Foo::g(int a) {}\nvoid Foo::run() {}\nvoid free1() {}\nvoid free2() {}\n",
        "int Foo::h() { return 1; }\nvoid Bar::f() {}\nvoid Foo::f() {}\n",
        "",
        "void Foo::f() {}\nvoid Foo::g(int a) {}\nvoid Foo::run() {}\nvoid free1() {}\n",
        "namespace n { void free1() {} }\nvoid Foo::run() {}\n",
    ];
    for (i, body) in edits.iter().enumerate() {
        idx.update(&p, Some(&a(body)));
        assert!(!idx.is_dirty(), "edit {i} fell back to a full rebuild");
        let fresh_ws = Ws::new(&[("h.h", h), ("a.cpp", &a(body))], &[]);
        let fresh = fresh_ws.index(&["a.cpp"]);
        assert_eq!(observable(&idx), observable(&fresh), "edit {i}");
    }
    // Many more edits: dead symbols pile up, and idle housekeeping compacts them without ever making an edit wait.
    let mut compactions = 0;
    for i in 0..120 {
        let body = edits[i % edits.len()];
        idx.update(&p, Some(&a(&format!("{body}int tick{i};\n"))));
        assert!(!idx.is_dirty(), "churn edit {i} fell back to a full rebuild");
        compactions += usize::from(idx.maintain());
        assert!(!idx.is_dirty());
        let fresh = Ws::new(&[("h.h", h), ("a.cpp", &a(&format!("{body}int tick{i};\n")))], &[]).index(&["a.cpp"]);
        assert_eq!(observable(&idx), observable(&fresh), "churn edit {i}");
    }
    assert!(compactions > 0 && compactions < 40, "{compactions} compactions");
}

#[test]
fn headers_are_discovered_and_indexed() {
    let ws = Ws::new(
        &[("a.cpp", "#include \"h/b.h\"\nvoid main_fn() {}\n"), ("h/b.h", "#pragma once\n#include \"c.h\"\nstruct B {};\n"), ("h/c.h", "struct C {};\n")],
        &[],
    );
    let idx = ws.index(&["a.cpp"]);
    for n in ["main_fn", "B", "C"] {
        assert_eq!(idx.db.lookup(n).len(), 1, "{n}");
    }
    assert_eq!(idx.live_files().count(), 3);
}

#[test]
fn base_class_prefers_same_file_over_same_named_class_in_unrelated_namespace() {
    let ws = Ws::new(
        &[
            ("a.cpp", "namespace Dummy { struct Test { int x; }; }\n"),
            ("b.cpp", "class Test {};\nclass D1 : public Test {};\nclass D2 : public Test {};\n"),
        ],
        &[],
    );
    let mut idx = ws.index(&["a.cpp", "b.cpp"]);
    let local = |idx: &Index| {
        let t = idx.db.lookup_id(idx.db.names.lookup("Test").unwrap()).iter().copied().find(|&i| idx.db.qualified_name(i) == "Test").unwrap();
        names(idx, idx.hier.derived(t))
    };
    assert_eq!(local(&idx).len(), 2);
    let p = ws.path("b.cpp");
    idx.update(&p, Some("class Test {};\nclass D5 : public Test {};\nclass D1 : public Test {};\nclass D2 : public Test {};\n"));
    idx.flush();
    assert_eq!(local(&idx).len(), 3);
}

#[test]
fn base_resolution_uses_scope_names_and_written_qualifiers() {
    let h = "#pragma once\nnamespace A { struct Base {}; }\nnamespace B { struct Base {}; }\nstruct Fwd;\nnamespace N { struct Fwd {}; }\n";
    let a = "#include \"h.h\"\nnamespace A { struct DA : Base {}; struct DB : B::Base {}; }\nnamespace B { struct DB2 : Base {}; struct DG : ::Fwd {}; }\nstruct G : Fwd {};\n";
    let ws = Ws::new(&[("h.h", h), ("a.cpp", a)], &[]);
    let mut idx = ws.index(&["a.cpp"]);
    let base_of = |idx: &Index, cls: &str| -> String {
        let id = (0..idx.db.len()).map(SymbolId::from_usize).find(|&i| !idx.db.is_dead(i) && idx.db.qualified_name(i) == cls).unwrap();
        names(idx, idx.hier.bases(id)).join(",")
    };
    let check = |idx: &Index| {
        assert_eq!(base_of(idx, "A::DA"), "A::Base");
        assert_eq!(base_of(idx, "A::DB"), "B::Base", "qualifier wins over the enclosing namespace");
        assert_eq!(base_of(idx, "B::DB2"), "B::Base", "namespace reopened in another file");
        assert_eq!(base_of(idx, "B::DG"), "Fwd", "::Fwd is the global one");
        assert_eq!(base_of(idx, "G"), "Fwd");
    };
    check(&idx);
    idx.update(&ws.path("a.cpp"), Some(&format!("{a}namespace B {{ struct Extra : Base {{}}; }}\n")));
    idx.flush();
    check(&idx);
    assert_eq!(base_of(&idx, "B::Extra"), "B::Base");
}

#[test]
fn update_replaces_file_symbols_and_removes_stale_ones() {
    let ws = Ws::new(&[("a.cpp", "struct Old {};\nvoid keep() {}\n"), ("b.cpp", "struct Other {};\n")], &[]);
    let mut idx = ws.index(&["a.cpp", "b.cpp"]);
    assert_eq!(idx.db.lookup("Old").len(), 1);

    idx.update(&ws.path("a.cpp"), Some("struct New {};\nvoid keep() {}\n"));
    idx.flush();
    assert!(idx.db.lookup("Old").is_empty());
    assert_eq!(idx.db.lookup("New").len(), 1);
    assert_eq!(idx.db.lookup("keep").len(), 1);
    assert_eq!(idx.db.lookup("Other").len(), 1);

    idx.remove(&ws.path("b.cpp"));
    idx.flush();
    assert!(idx.db.lookup("Other").is_empty());
    assert!(idx.file_index(&ws.path("b.cpp")).is_none());
}

#[test]
fn offset_only_edits_patch_in_place_but_real_changes_do_not() {
    let pad: String = (0..300).map(|i| format!("int pad{i};\n")).collect();
    let base = "struct B { virtual void f(int); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a) { }\n";
    let ws = Ws::new(&[("a.cpp", base), ("pad.cpp", &pad)], &[]);
    let mut idx = ws.index(&["a.cpp", "pad.cpp"]);
    let a = ws.path("a.cpp");

    // Moving declarations only: patched, still clean, offsets follow.
    idx.update(&a, Some(&format!("// pad\n\n{base}")));
    assert!(!idx.is_dirty());
    let g = idx.db.lookup("g")[0];
    assert_eq!(idx.db.symbol(g).name_offset as usize, format!("// pad\n\n{base}").find("g(int").unwrap());

    // Each of these changes the shape, and is still patched in place.
    let changes = [
        ("struct B { virtual void f(int); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a, int b) { }\n", "arity"),
        ("struct B { virtual void f(int); };\nstruct D { void f(int) override; int v; };\nvoid g(int a) { }\n", "base removed"),
        ("struct B { virtual void f(int); };\nstruct D : B { void f(int) override; private: int v; };\nvoid g(int a) { }\n", "access"),
        ("struct B { void f(int); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a) { }\n", "virtual flag"),
        ("struct B { virtual void f(int); };\nstruct D : B { void f(int) override; };\nint v; void g(int a) { }\n", "moved out of struct"),
        ("struct B { virtual void f(long); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a) { }\n", "param type"),
        ("struct B { virtual void f(int); };\nstruct C : B { void f(int); };\nstruct D : C { void f(int) override; int v; };\nvoid g(int a) { }\n", "class inserted"),
        ("struct D : B { void f(int) override; int v; };\nvoid g(int a) { }\n", "base removed entirely"),
        ("struct B { virtual void f(int); virtual ~B(); };\nstruct D : B { void f(int) override; ~D(); };\nvoid B::f(int) {}\nvoid D::f(int) {}\nB::~B() {}\n", "dtors and defs"),
    ];
    for (src, what) in changes {
        let mut idx = ws.index(&["a.cpp", "pad.cpp"]);
        idx.update(&a, Some(src));
        assert!(!idx.is_dirty(), "{what}: fell back to a full rebuild");
        let fresh = Ws::new(&[("a.cpp", src), ("pad.cpp", &pad)], &[]).index(&["a.cpp", "pad.cpp"]);
        assert_eq!(observable(&idx), observable(&fresh), "{what}");
    }
}

#[test]
fn header_edits_patch_class_hierarchy_across_files() {
    let pad: String = (0..300).map(|i| format!("int pad{i};\n")).collect();
    let base = "#pragma once\n".to_string();
    let h = |body: &str| format!("{base}{body}");
    let v0 = "struct A { virtual void f(); virtual void g(int); };\nstruct B : A { void f() override; };\n";
    let ws = Ws::new(
        &[
            ("pad.h", &format!("#pragma once\n{pad}")),
            ("h.h", &h(v0)),
            ("d.h", "#pragma once\n#include \"h.h\"\nstruct C : B { void f() override; void g(int) override; };\nstruct E : A { void g(int) override; };\n"),
            ("a.cpp", "#include \"pad.h\"\n#include \"d.h\"\nvoid A::f() {}\nvoid B::f() {}\nvoid C::f() {}\nvoid E::g(int) {}\n"),
        ],
        &[],
    );
    let mut idx = ws.index(&["a.cpp"]);
    let p = ws.path("h.h");
    let edits = [
        "struct A { virtual void f(); virtual void g(int); virtual void h(); };\nstruct B : A { void f() override; };\n",
        "struct A { virtual void f(); void g(int); };\nstruct B : A { void f() override; };\n",
        "struct A { virtual void f(); virtual void g(int); };\nstruct B { void f() override; };\n",
        "struct A { virtual void f(); virtual void g(int); };\nstruct B : A { void f() override; };\nstruct B2 : B {};\n",
        "struct A2 { virtual void f(); };\nstruct A : A2 { void f() override; virtual void g(int); };\nstruct B : A { void f() override; };\n",
        "struct B : A { void f() override; };\n",
        v0,
    ];
    for (i, body) in edits.iter().enumerate() {
        idx.update(&p, Some(&h(body)));
        assert!(!idx.is_dirty(), "edit {i} fell back to a full rebuild");
        let fresh_ws = Ws::new(
            &[
                ("pad.h", &format!("#pragma once\n{pad}")),
                ("h.h", &h(body)),
                ("d.h", "#pragma once\n#include \"h.h\"\nstruct C : B { void f() override; void g(int) override; };\nstruct E : A { void g(int) override; };\n"),
                ("a.cpp", "#include \"pad.h\"\n#include \"d.h\"\nvoid A::f() {}\nvoid B::f() {}\nvoid C::f() {}\nvoid E::g(int) {}\n"),
            ],
            &[],
        );
        let fresh = fresh_ws.index(&["a.cpp"]);
        let (l, r) = (observable(&idx), observable(&fresh));
        let diff = |a: &[String], b: &[String]| a.iter().filter(|x| !b.contains(x)).cloned().collect::<Vec<_>>();
        assert!(l == r, "edit {i}\nincremental only: {:#?}\nfresh only: {:#?}", diff(&l, &r), diff(&r, &l));
    }
}

#[test]
fn update_from_disk_and_new_file_with_new_header() {
    let ws = Ws::new(&[("a.cpp", "void a() {}\n")], &[]);
    let mut idx = ws.index(&["a.cpp"]);
    ws.write("new.h", "struct Fresh {};\n");
    ws.write("new.cpp", "#include \"new.h\"\nvoid n() {}\n");
    idx.update(&ws.path("new.cpp"), None);
    idx.flush();
    assert_eq!(idx.db.lookup("Fresh").len(), 1);
    assert_eq!(idx.db.lookup("n").len(), 1);

    std::fs::remove_file(ws.dir.path().join("new.cpp")).unwrap();
    idx.update(&ws.path("new.cpp"), None);
    assert!(!idx.is_dirty(), "removing a file patches the indexes");
    assert!(idx.db.lookup("n").is_empty());
}

#[test]
fn hierarchy_survives_updates_and_compaction() {
    let ws = Ws::new(
        &[("t.h", "#pragma once\nstruct B { virtual void f(); };\nstruct D : B { void f() override; };\n"), ("a.cpp", "#include \"t.h\"\n")],
        &[],
    );
    let mut idx = ws.index(&["a.cpp"]);
    for i in 0..5 {
        idx.update(&ws.path("t.h"), Some(&format!("#pragma once\nstruct B {{ virtual void f(); }};\nstruct D : B {{ void f() override; }};\nint v{i};\n")));
        idx.flush();
    }
    let fs = idx.db.lookup("f");
    assert_eq!(fs.len(), 2);
    assert_eq!(idx.hier.family_members(&idx.db, fs[0]).len(), 2);
    assert!(idx.db.lookup("v3").is_empty());
    assert_eq!(idx.db.lookup("v4").len(), 1);
    assert!(idx.dead_symbols() < idx.db.len());
}

#[test]
fn parallel_build_is_deterministic() {
    let files: Vec<(String, String)> = (0..40)
        .map(|i| (format!("f{i}.cpp"), format!("#include \"common.h\"\nstruct T{i} : Common {{ void m{i}(); }};\nvoid T{i}::m{i}() {{}}\n")))
        .collect();
    let mut all: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    all.push(("common.h", "#pragma once\nstruct Common { virtual ~Common(); };\n"));
    let ws = Ws::new(&all, &[]);
    let tus: Vec<&str> = files.iter().map(|(a, _)| a.as_str()).collect();
    let dump = |idx: &Index| -> Vec<String> { idx.db.symbols().ids().map(|i| format!("{}@{}", idx.db.qualified_name(i), idx.db.file_path(i))).collect() };
    let a = dump(&ws.index(&tus));
    let b = dump(&ws.index(&tus));
    assert_eq!(a, b);
    assert!(a.len() > 80);
}
