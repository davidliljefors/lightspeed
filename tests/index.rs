//! Index build/update and navigation queries against real files on disk.

use lightspeed::compile_commands::CompileDb;
use lightspeed::index::{Index, IndexConfig};
use lightspeed::query::{DiskText, Query};
use lightspeed::symbols::SymbolId;

struct Ws {
    dir: tempfile::TempDir,
    root: String,
}

impl Ws {
    fn new(files: &[(&str, &str)], _tus: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().replace('\\', "/");
        let ws = Self { dir, root };
        for (p, t) in files {
            ws.write(p, t);
        }
        ws
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn path(&self, rel: &str) -> String {
        format!("{}/{rel}", self.root)
    }

    fn index(&self, tus: &[&str]) -> Index {
        let entries: Vec<String> = tus
            .iter()
            .map(|f| format!(r#"{{"directory":"{}","file":"{f}","arguments":["clang++","-c","{f}","-I","{}"]}}"#, self.root, self.root))
            .collect();
        let cdb = CompileDb::from_json(&format!("[{}]", entries.join(","))).unwrap();
        Index::build(cdb, IndexConfig::default())
    }
}

/// Strips the `$` marker and returns the text with its byte offset.
fn cursor(src: &str) -> (String, u32) {
    let at = src.find('$').expect("marker");
    (src.replacen('$', "", 1), at as u32)
}

fn names(idx: &Index, ids: &[SymbolId]) -> Vec<String> {
    ids.iter().map(|&i| idx.db.qualified_name(i)).collect()
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
fn definition_picks_overload_by_arity_and_scope() {
    let src = "struct Foo { void f(int a); void f(int a, int b); void g(); };\n\
               void Foo::g() { f(1, 2); }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let (text, off) = cursor("struct Foo { void f(int a); void f(int a, int b); void g(); };\nvoid Foo::g() { $f(1, 2); }\n");
    let defs = q.definition(&ws.path("a.cpp"), &text, off);
    assert_eq!(defs.len(), 1);
    assert_eq!(idx.db.signature(defs[0]).unwrap().arity, 2);
}

#[test]
fn definition_prefers_out_of_line_implementation() {
    let ws = Ws::new(
        &[
            ("foo.h", "#pragma once\nstruct Foo { void run(); };\n"),
            ("foo.cpp", "#include \"foo.h\"\nvoid Foo::run() {}\n"),
            ("use.cpp", "#include \"foo.h\"\nvoid go(Foo& f) { f.run(); }\n"),
        ],
        &[],
    );
    let idx = ws.index(&["foo.cpp", "use.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let text = std::fs::read_to_string(ws.path("use.cpp")).unwrap();
    let off = text.find("run").unwrap() as u32;
    let defs = q.definition(&ws.path("use.cpp"), &text, off);
    assert_eq!(defs.len(), 1);
    let loc = q.location(defs[0]).unwrap();
    assert!(idx.files[loc.file as usize].path.ends_with("foo.cpp"));
}

#[test]
fn qualified_name_selects_namespace() {
    let src = "namespace A { void g(); }\nnamespace B { void g(); }\nvoid h() { A::g(); }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let off = src.rfind("g()").unwrap() as u32;
    let defs = q.definition(&ws.path("a.cpp"), src, off);
    assert_eq!(names(&idx, &defs), ["A::g"]);
}

#[test]
fn class_definition_beats_forward_declaration() {
    let src = "struct W;\nstruct W { int x; };\nvoid f(W* w);\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let off = src.find("(W*").unwrap() as u32 + 1;
    let defs = q.definition(&ws.path("a.cpp"), src, off);
    assert_eq!(defs.len(), 1);
    assert!(idx.db.symbol(defs[0]).flags.contains(lightspeed::symbols::SymbolFlags::DEFINITION));
}

#[test]
fn references_cover_base_and_overrides() {
    let ws = Ws::new(
        &[
            ("shapes.h", "#pragma once\nstruct Shape { virtual float area(); };\nstruct Sq : Shape { float area() override; };\n"),
            ("a.cpp", "#include \"shapes.h\"\nfloat Sq::area() { return 1; }\nfloat total(Shape* s) { return s->area(); }\n"),
            ("b.cpp", "int unrelated() { return 0; }\n"),
        ],
        &[],
    );
    let idx = ws.index(&["a.cpp", "b.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let refs = q.references("area");
    assert_eq!(refs.len(), 4);
    let files: std::collections::BTreeSet<_> = refs.iter().map(|r| idx.files[r.file as usize].path.rsplit('/').next().unwrap().to_owned()).collect();
    assert_eq!(files.into_iter().collect::<Vec<_>>(), ["a.cpp", "shapes.h"]);
}

#[test]
fn references_ignore_comments_and_strings() {
    let src = "// area\nconst char* s = \"area\";\nint area;\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    assert_eq!(Query::new(&idx, &DiskText).references("area").len(), 1);
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
    let base = "struct B { virtual void f(int); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a) { }\n";
    let ws = Ws::new(&[("a.cpp", base)], &[]);
    let mut idx = ws.index(&["a.cpp"]);
    let a = ws.path("a.cpp");

    // Moving declarations only: patched, still clean, offsets follow.
    idx.update(&a, Some(&format!("// pad\n\n{base}")));
    assert!(!idx.is_dirty());
    let g = idx.db.lookup("g")[0];
    assert_eq!(idx.db.symbol(g).name_offset as usize, format!("// pad\n\n{base}").find("g(int").unwrap());

    // Each of these changes the shape and must go through the full path.
    let changes = [
        ("struct B { virtual void f(int); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a, int b) { }\n", "arity"),
        ("struct B { virtual void f(int); };\nstruct D { void f(int) override; int v; };\nvoid g(int a) { }\n", "base removed"),
        ("struct B { virtual void f(int); };\nstruct D : B { void f(int) override; private: int v; };\nvoid g(int a) { }\n", "access"),
        ("struct B { void f(int); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a) { }\n", "virtual flag"),
        ("struct B { virtual void f(int); };\nstruct D : B { void f(int) override; };\nint v; void g(int a) { }\n", "moved out of struct"),
        ("struct B { virtual void f(long); };\nstruct D : B { void f(int) override; int v; };\nvoid g(int a) { }\n", "param type"),
    ];
    for (src, what) in changes {
        let mut idx = ws.index(&["a.cpp"]);
        idx.update(&a, Some(src));
        assert!(idx.is_dirty(), "{what}: was patched in place");
        idx.flush();
        let fresh = Ws::new(&[("a.cpp", src)], &[]).index(&["a.cpp"]);
        assert_eq!(idx.db.len() - idx.db.dead_count(), fresh.db.len(), "{what}");
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
    idx.flush();
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
fn completion_scope_receiver_and_this() {
    let decl = "struct Base { int inherited; };\n\
                struct Foo : Base { int alpha; void beta(); void run(); };\n\
                void Foo::run() { this->$ }\n";
    let ws = Ws::new(&[("a.cpp", &decl.replace('$', ""))], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");

    let (t, o) = cursor(decl);
    let (items, _) = q.completions(&p, &t, o, 100);
    let n: std::collections::BTreeSet<_> = items.iter().map(|&i| idx.db.name(i).to_owned()).collect();
    assert!(n.contains("alpha") && n.contains("beta") && n.contains("inherited"), "{n:?}");

    let local = "struct Foo { int alpha; void beta(); };\nvoid f() { Foo x; x.al$ }\n";
    let ws2 = Ws::new(&[("a.cpp", &local.replace('$', ""))], &[]);
    let idx2 = ws2.index(&["a.cpp"]);
    let (t, o) = cursor(local);
    let (items, _) = Query::new(&idx2, &DiskText).completions(&ws2.path("a.cpp"), &t, o, 100);
    assert_eq!(items.iter().map(|&i| idx2.db.name(i)).collect::<Vec<_>>(), ["alpha"]);

    let qual = "namespace N { struct S {}; void fn(); }\nvoid g() { N::$ }\n";
    let ws3 = Ws::new(&[("a.cpp", &qual.replace('$', ""))], &[]);
    let idx3 = ws3.index(&["a.cpp"]);
    let (t, o) = cursor(qual);
    let (items, _) = Query::new(&idx3, &DiskText).completions(&ws3.path("a.cpp"), &t, o, 100);
    let n: std::collections::BTreeSet<_> = items.iter().map(|&i| idx3.db.name(i).to_owned()).collect();
    assert_eq!(n.into_iter().collect::<Vec<_>>(), ["S", "fn"]);
}

#[test]
fn receiver_types_via_auto_calls_chains_and_smart_pointers() {
    const PRE: &str = "namespace eastl { template<class T> struct unique_ptr { T* get(); }; template<class T, class... A> unique_ptr<T> make_unique(A...); \
                       template<class T> struct vector { T& front(); }; }\n\
                       struct A { int aa; };\nstruct B { int bb; A* link; A make_a(); A& ref(); eastl::unique_ptr<A> owned; eastl::vector<A> items; };\n\
                       struct C { int cc; };\nB* get_b();\nB make_b(int);\nstruct V { V operator+(V); int vv; };\n\
                       struct Handle { A* operator->(); A& operator*(); B& operator[](int); };\nstruct Bag { A* begin(); A* end(); };\n\
                       struct Holder : C { B member; B* pmember; B getB(); void run(); };\n";
    let cases: &[(&str, &str, &[&str])] = &[
        ("plain", "void f(B b) { b.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("auto from ctor", "void f() { auto x = B(1); x.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("auto from call", "void f() { auto x = make_b(2); x.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("auto ptr from call", "void f() { auto* x = get_b(); x->$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("auto new", "void f() { auto x = new A; x->$ }", &["aa"]),
        ("auto from auto", "void f(B b) { auto x = b; auto& y = x; y.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("auto make_unique", "void f() { auto p = eastl::make_unique<A>(); p->$ }", &["aa"]),
        ("smart ptr member", "void f(B b) { b.owned->$ }", &["aa"]),
        ("smart ptr dot", "void f(B b) { b.owned.$ }", &["get"]),
        ("field chain", "void f(B b) { b.link->$ }", &["aa"]),
        ("call chain", "void f(B b) { b.make_a().$ }", &["aa"]),
        ("ref chain", "void f(B* b) { b->ref().$ }", &["aa"]),
        ("chain via auto", "void f() { auto x = get_b(); x->make_a().$ }", &["aa"]),
        ("subscript", "void f(B b) { b.items[0].$ }", &["aa"]),
        ("this member", "void Holder::run() { member.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("this call", "void Holder::run() { getB().link->$ }", &["aa"]),
        ("inherited via this", "void Holder::run() { this->$ }", &["cc", "member", "pmember", "getB", "run"]),
        ("range-for container member", "void f(B b) { for (auto& e : b.items) { e.$ } }", &["aa"]),
        ("range-for c array", "void f() { A arr[4]; for (const auto& e : arr) e.$ }", &["aa"]),
        ("range-for via call", "void f() { for (auto e : get_b()->items) e.$ }", &["aa"]),
        ("range-for user begin", "void f(Bag g) { for (auto& e : g) e.$ }", &["aa"]),
        ("range-for explicit type", "void f(B b) { for (A& e : b.items) e.$ }", &["aa"]),
        ("operator->", "void f(Handle h) { h->$ }", &["aa"]),
        ("deref", "void f(Handle h) { (*h).$ }", &["aa"]),
        ("deref via auto", "void f(Handle h) { auto& r = *h; r.$ }", &["aa"]),
        ("operator[]", "void f(Handle h) { h[0].$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("operator[] via arrow chain", "void f(Handle h) { h[0].link->$ }", &["aa"]),
        ("binary operator", "void f(V a, V b) { auto s = a + b; s.$ }", &["vv", "operator+"]),
        ("ternary", "void f(bool c, B b) { auto x = c ? b : make_b(1); x.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("c cast", "void f(void* p) { ((B*)p)->$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("static_cast", "void f(void* p) { static_cast<A*>(p)->$ }", &["aa"]),
        ("static_cast via auto", "void f(void* p) { auto x = static_cast<A*>(p); x->$ }", &["aa"]),
        ("move", "void f(B b) { auto x = eastl::move(b); x.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
        ("assignment", "void f(B b) { B y; auto x = y = b; x.$ }", &["bb", "link", "make_a", "ref", "owned", "items"]),
    ];
    for (name, body, want) in cases {
        let marked = format!("{PRE}{body}\n");
        let ws = Ws::new(&[("a.cpp", &marked.replace('$', ""))], &[]);
        let idx = ws.index(&["a.cpp"]);
        let (t, o) = cursor(&marked);
        let (items, _) = Query::new(&idx, &DiskText).completions(&ws.path("a.cpp"), &t, o, 100);
        let got: std::collections::BTreeSet<_> = items.iter().map(|&i| idx.db.name(i)).collect();
        let want: std::collections::BTreeSet<_> = want.iter().copied().collect();
        assert_eq!(got, want, "case: {name}");
    }
}

#[test]
fn completion_summaries_show_types_and_signatures() {
    let src = "struct T { virtual const char* name(int a, float b = 1.0f) const = 0; static constexpr int kMax = 4; eastl::vector<int> items; T(int x) : v(x) {} int v; };\n\
               #define MAX_ITEMS 16\nenum class E { One = 1, Two };\nclass Foo : public T { };\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let by = |n: &str| idx.db.lookup(n).to_vec();
    let summary = |n: &str| {
        let ids = by(n);
        q.summaries(&ids[..1]).remove(0).unwrap_or_else(|| panic!("no summary for {n}"))
    };
    assert_eq!(summary("name"), "virtual const char* name(int a, float b = 1.0f) const = 0");
    assert_eq!(summary("kMax"), "static constexpr int kMax");
    assert_eq!(summary("items"), "eastl::vector<int> items");
    assert_eq!(summary("v"), "int v");
    assert_eq!(summary("Foo"), "class Foo");
    assert_eq!(summary("MAX_ITEMS"), "#define MAX_ITEMS 16");
}

#[test]
fn typedef_struct_declarators_are_indexed() {
    let src = "typedef struct _SEC {\n int a;\n} SEC, *PSEC;\ntypedef union _U { int a; } U;\nvoid f(const SEC& s);\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    for n in ["_SEC", "SEC", "PSEC", "U"] {
        assert_eq!(idx.db.lookup(n).len(), 1, "{n}");
    }
    let q = Query::new(&idx, &DiskText);
    let defs = q.definition(&ws.path("a.cpp"), src, src.find("SEC&").unwrap() as u32);
    assert_eq!(defs.len(), 1);
}

#[test]
fn typedef_struct_with_anonymous_union_and_macro_attrs() {
    let src = "#define UNALIGNED\n#include \"w.h\"\nvoid f(const IMAGE_RELOCATION& r, PIMAGE_RELOCATION p);\n";
    let w = "typedef unsigned long DWORD; typedef unsigned short WORD;\n#pragma pack(push, 2)\n\
             typedef struct _IMAGE_RELOCATION {\n    union {\n        DWORD   VirtualAddress;\n        DWORD   RelocCount;   // c\n    } DUMMYUNIONNAME;\n    DWORD   SymbolTableIndex;\n    WORD    Type;\n} IMAGE_RELOCATION;\n\
             typedef IMAGE_RELOCATION UNALIGNED *PIMAGE_RELOCATION;\n#pragma pack(pop)\n";
    let ws = Ws::new(&[("a.cpp", src), ("w.h", w)], &[]);
    let idx = ws.index(&["a.cpp"]);
    for n in ["IMAGE_RELOCATION", "PIMAGE_RELOCATION", "SymbolTableIndex"] {
        assert_eq!(idx.db.lookup(n).len(), 1, "{n}");
    }
    let q = Query::new(&idx, &DiskText);
    let defs = q.definition(&ws.path("a.cpp"), src, src.find("IMAGE_RELOCATION&").unwrap() as u32);
    assert_eq!(defs.len(), 1);
}

#[test]
fn member_access_through_typedef_alias_and_case_macros() {
    let w = "#pragma once\ntypedef unsigned short WORD;\n#define IMAGE_REL_ABSOLUTE 0x0000\n#define IMAGE_REL_ADDR64 0x0001\n\
             typedef struct _IMAGE_RELOCATION {\n    union { unsigned VirtualAddress; unsigned RelocCount; };\n    unsigned SymbolTableIndex;\n    WORD Type;\n} IMAGE_RELOCATION;\n\
             struct Other { int Type; };\nusing Alias = IMAGE_RELOCATION;\n";
    let a = "#include \"w.h\"\nvoid apply(const IMAGE_RELOCATION& relocation, Alias* al, Other o) {\n    switch (relocation.Type) {\n    case IMAGE_REL_ADDR64: { al->RelocCount; o.Type; break; }\n    }\n}\n";
    let ws = Ws::new(&[("a.cpp", a), ("w.h", w)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |needle: &str, nth: usize| a.match_indices(needle).nth(nth).unwrap().0 as u32 + 1;

    let d = q.definition(&p, a, at("Type", 0));
    assert_eq!(names(&idx, &d), ["_IMAGE_RELOCATION::Type"]);
    let d = q.definition(&p, a, at("Type", 1));
    assert_eq!(names(&idx, &d), ["Other::Type"]);
    let d = q.definition(&p, a, at("RelocCount", 0));
    assert_eq!(names(&idx, &d), ["_IMAGE_RELOCATION::RelocCount"]);
    let d = q.definition(&p, a, at("IMAGE_REL_ADDR64", 0));
    assert_eq!(names(&idx, &d), ["IMAGE_REL_ADDR64"]);
    assert_eq!(idx.db.symbol(d[0]).kind, lightspeed::symbols::SymbolKind::Macro);
}

#[test]
fn symbol_refs_follow_the_symbol_not_the_spelling() {
    let src = "struct Base { virtual void tick(int dt); int count; Base(); ~Base(); };\n\
struct Other { void tick(int dt); int count; };\n\
struct Thing : Base { void tick(int dt) override; };\n\
Base::Base() {}\nBase::~Base() {}\n\
void Base::tick(int dt) { count += dt; }\n\
void Thing::tick(int dt) { Base::tick(dt); }\n\
int use(Thing* t, Other& o, Base* b) { b->tick(1); t->tick(2); o.tick(3); o.count; return t->count; }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let nth = |needle: &str, n: usize| src.match_indices(needle).nth(n).unwrap().0 as u32;
    let starts = |r: &lightspeed::query::SymbolRefs| r.certain.iter().map(|l| l.start).collect::<Vec<_>>();

    // Virtual family: base, override, both out-of-line definitions, qualified call and virtual calls; not Other::tick.
    for from in [0, 2, 3, 6] {
        let r = q.symbol_refs(&p, src, nth("tick", from) + 1).unwrap();
        let want: Vec<u32> = [0, 2, 3, 4, 5, 6, 7].iter().map(|&n| nth("tick", n)).collect();
        assert_eq!(starts(&r), want, "from occurrence {from}");
        assert!(r.uncertain.is_empty(), "{:?}", r.uncertain);
    }
    let r = q.symbol_refs(&p, src, nth("tick", 1) + 1).unwrap();
    assert_eq!(starts(&r), [nth("tick", 1), nth("tick", 8)]);

    // Fields: Base::count is used unqualified in Base::tick and through Thing*; Other::count is separate.
    let r = q.symbol_refs(&p, src, nth("count", 0) + 1).unwrap();
    assert_eq!(starts(&r), [nth("count", 0), nth("count", 2), nth("count", 4)]);

    // A class renames together with its constructor and destructor.
    let r = q.symbol_refs(&p, src, nth("Base", 0) + 1).unwrap();
    let want: Vec<u32> = (0..11).map(|n| nth("Base", n)).collect();
    assert_eq!(starts(&r), want);

    // Parameters are not symbols.
    assert!(q.symbol_refs(&p, src, nth("dt", 0) + 1).is_none());
}

#[test]
fn symbol_refs_cover_macros_and_enum_constants() {
    let hdr = "#pragma once\n#define MAX_ITEMS 16\n#define SQUARE(x) ((x) * (x))\nenum class Mode { Fast, Slow };\nenum Plain { Fast, Other };\n";
    let src = "#include \"h.h\"\nint a[MAX_ITEMS];\nint f(int v) { return SQUARE(v) + MAX_ITEMS; }\n\
Mode m = Mode::Fast; Plain p = Fast; int g(Mode x) { switch (x) { case Mode::Slow: return 1; case Mode::Fast: return 2; } return 0; }\n\
#ifdef MAX_ITEMS\nint y;\n#endif\n#undef MAX_ITEMS\n";
    let ws = Ws::new(&[("a.cpp", src), ("h.h", hdr)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let (p, hp) = (ws.path("a.cpp"), ws.path("h.h"));
    let nth = |s: &str, needle: &str, n: usize| s.match_indices(needle).nth(n).unwrap().0 as u32;
    let count = |r: &lightspeed::query::SymbolRefs| r.certain.len();

    // From the use and from the definition, every spelling in code, #ifdef and #undef is covered.
    for (path, text, n) in [(&p, src, 0), (&p, src, 1), (&hp, hdr, 0)] {
        let r = q.symbol_refs(path, text, nth(text, "MAX_ITEMS", n) + 1).unwrap();
        assert_eq!(count(&r), 5, "from {n}: {r:?}", r = r.certain);
    }
    let r = q.symbol_refs(&p, src, nth(src, "SQUARE", 0) + 1).unwrap();
    assert_eq!(count(&r), 2);

    // Enum constants of different enums stay apart.
    let r = q.symbol_refs(&p, src, nth(src, "Mode::Fast", 0) + 7).unwrap();
    assert_eq!(count(&r), 3, "{:?}", r.certain);
    let r = q.symbol_refs(&p, src, nth(src, "Fast;", 1) + 1).unwrap();
    assert_eq!(count(&r), 2, "{:?}", r.certain);
}

#[test]
fn hover_reports_declaration_docs_and_hierarchy() {
    let src = "// Base of all things.\n// Second line.\nstruct Base { virtual void tick(int dt); };\n\
               /** Derived thing. */\nstruct Thing : Base { void tick(int dt) override; int count; };\n\
               void use(Thing* t) { t->tick(1); t->count; }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |needle: &str, nth: usize| src.match_indices(needle).nth(nth).unwrap().0 as u32 + 1;

    let h = q.hover(&p, src, at("tick", 2)).unwrap();
    assert_eq!(h.signature, "virtual void tick(int dt)");
    assert_eq!(h.container, "Base");
    assert!(h.notes.iter().any(|n| n == "virtual") && h.notes.iter().any(|n| n.contains("family of 2")), "{:?}", h.notes);

    let h = q.hover(&p, src, at("count", 1)).unwrap();
    assert_eq!(h.signature, "int count");

    let h = q.hover(&p, src, at("Thing", 1)).unwrap();
    assert_eq!(h.signature, "struct Thing");
    assert_eq!(h.doc.as_deref(), Some("Derived thing."));
    assert!(h.notes.iter().any(|n| n == "bases: Base"), "{:?}", h.notes);

    let h = q.hover(&p, src, at("Base {", 0)).unwrap();
    assert_eq!(h.doc.as_deref(), Some("Base of all things.\nSecond line."));
    assert!(h.notes.iter().any(|n| n.contains("1 direct subclass")), "{:?}", h.notes);

    assert!(q.hover(&p, "   ", 1).is_none());
}

#[test]
fn parameters_and_locals_bind_lexically() {
    let src = "struct S { int name; void find(const char* name) const; int get(int x); };\n\
int name;\n\
void S::find(const char* name) const {\n  use(name);\n  { int name = 1; name++; }\n  name;\n}\n\
int S::get(int x) {\n  int a = x, b = 2;\n  for (auto i = 0; i < b; ++i) { a += i; }\n  auto fn = [x](int y, int z) { return x + y + z; };\n  if (auto p = &a) return *p + this->name + o.name;\n  return a;\n}\n\
S::S(int name) : name(name) { }\n\
void decl_only(Foo& item, int);\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |needle: &str, nth: usize| src.match_indices(needle).nth(nth).unwrap().0 as u32 + 1;
    let uses = |needle: &str, nth: usize| q.local_at(&p, src, at(needle, nth)).map(|l| l.uses.len());

    // `name` occurrences: 0 field, 1 param in the class decl, 2 global, 3 param decl, 4 use, 5-6 inner local, 7 use of param, 8-9 member accesses.
    assert_eq!(uses("name", 0), None);
    assert_eq!(uses("name", 1), Some(1)); // declaration-only parameter
    assert_eq!(uses("name", 2), None);
    for n in [3, 4, 7] {
        assert_eq!(uses("name", n), Some(3), "param occurrence {n}"); // decl + use(name) + trailing `name;`
    }
    for n in [5, 6] {
        assert_eq!(uses("name", n), Some(2), "inner local occurrence {n}"); // `int name = 1; name++`
    }
    assert_eq!(uses("name", 8), None);
    assert_eq!(uses("name", 9), None);
    let h = q.hover(&p, src, at("name", 4)).unwrap();
    assert_eq!((h.signature.as_str(), h.kind.as_str(), h.container.as_str()), ("const char* name", "parameter", "S::find"));

    // Multiple declarators, for-init, lambda params, if-init.
    assert_eq!(uses("a = x", 0), Some(4)); // decl, a += i, &a, return a
    assert_eq!(uses("b = 2", 0), Some(2));
    assert_eq!(uses("i = 0", 0), Some(4));
    assert_eq!(uses(" x) {", 0), Some(4)); // param, `a = x`, lambda capture, lambda body use
    assert_eq!(uses(" y,", 0), Some(2));
    assert_eq!(uses("p = &a", 0), Some(2));
    let h = q.hover(&p, src, at("b = 2", 0)).unwrap();
    assert_eq!((h.signature.as_str(), h.kind.as_str()), ("int b", "local variable"));
    let h = q.hover(&p, src, at("p = &a", 0)).unwrap();
    assert_eq!(h.signature, "auto p");

    // Constructor initializer list: the member is not the parameter.
    let ctor = src.find("S::S(int name)").unwrap();
    let param = (ctor + "S::S(int ".len()) as u32;
    assert_eq!(q.local_at(&p, src, param).unwrap().uses.len(), 2); // decl + `(name)` argument
    let member = (ctor + "S::S(int name) : ".len()) as u32;
    assert!(q.local_at(&p, src, member).is_none());

    // Unnamed parameters are not declarations.
    assert_eq!(uses("item", 0), Some(1));
    assert!(q.local_at(&p, src, at("int)", 0)).is_none());
}

#[test]
fn definition_follows_auto_range_for_and_operators() {
    let src = "struct A { int val; };\nstruct B { int val; };\nstruct Vec { A* begin(); A* end(); };\nstruct H { B* operator->(); };\nVec make();\n\
               void f(H h, Vec v) {\n  for (auto& e : v) e.val;\n  auto k = *h.operator->();\n  h->val;\n  auto m = make();\n  for (auto& q : m) q.val;\n}\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |needle: &str, nth: usize| src.match_indices(needle).nth(nth).unwrap().0 as u32 + 1;
    // Occurrences of `val`: 0,1 decls; 2 in `e.val`; 3 in `h->val`; 4 in `q.val`.
    assert_eq!(names(&idx, &q.definition(&p, src, at("val", 2))), ["A::val"]);
    assert_eq!(names(&idx, &q.definition(&p, src, at("val", 3))), ["B::val"]);
    assert_eq!(names(&idx, &q.definition(&p, src, at("val", 4))), ["A::val"]);
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
