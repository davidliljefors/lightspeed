//! Index build/update and navigation queries against real files on disk.

mod common;

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
    // Many more edits: dead symbols pile up until a compaction forces a full rebuild.
    let mut rebuilt = 0;
    for i in 0..120 {
        let body = edits[i % edits.len()];
        idx.update(&p, Some(&a(&format!("{body}int tick{i};\n"))));
        rebuilt += usize::from(idx.is_dirty());
        idx.flush();
        let fresh = Ws::new(&[("h.h", h), ("a.cpp", &a(&format!("{body}int tick{i};\n")))], &[]).index(&["a.cpp"]);
        assert_eq!(observable(&idx), observable(&fresh), "churn edit {i}");
    }
    assert!(rebuilt > 0 && rebuilt < 40, "{rebuilt} full rebuilds");
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
fn definition_skips_forward_declarations_across_files_and_scopes() {
    let ws = Ws::new(
        &[
            ("fwd.h", "#pragma once\nnamespace gfx { class Mesh; struct Vertex; enum class Kind : int; template <class T> class Vec; }\nclass Window;\n"),
            ("mesh.h", "#pragma once\nnamespace gfx {\nclass Mesh { public: int n; };\nstruct Vertex { float x; };\nenum class Kind : int { A, B };\ntemplate <class T> class Vec { T v; };\n}\n"),
            ("window.h", "#pragma once\nclass Window { public: void show(); };\n"),
            (
                "use.cpp",
                "#include \"fwd.h\"\n#include \"mesh.h\"\n#include \"window.h\"\nclass Window;\nnamespace gfx { class Mesh; }\n\
                 void go(gfx::Mesh* m, gfx::Vertex* v, gfx::Kind k, gfx::Vec<int>* ve, Window* w, class Window* w2) {}\n",
            ),
            ("only.cpp", "class Lonely;\nvoid f(Lonely* l);\n"),
        ],
        &[],
    );
    let idx = ws.index(&["use.cpp", "only.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("use.cpp");
    let text = std::fs::read_to_string(&p).unwrap();
    let go = text.find("void go").unwrap();
    let at = |s: &str, nth: usize| (go + text[go..].match_indices(s).nth(nth).unwrap().0) as u32;
    let is_def = |id| idx.db.symbol(id).flags.contains(lightspeed::symbols::SymbolFlags::DEFINITION);
    let file_of = |id| idx.files[q.location(id).unwrap().file as usize].path.rsplit('/').next().unwrap().to_owned();
    for (what, off, file) in [
        ("Mesh", at("Mesh", 0), "mesh.h"),
        ("Vertex", at("Vertex", 0), "mesh.h"),
        ("Kind", at("Kind", 0), "mesh.h"),
        ("Vec", at("Vec", 0), "mesh.h"),
        ("Window", at("Window*", 0), "window.h"),
        ("class Window", at("Window*", 1), "window.h"),
    ] {
        let defs = q.definition(&p, &text, off);
        assert!(!defs.is_empty(), "{what}");
        assert!(defs.iter().all(|&d| is_def(d)), "{what} resolved to a forward declaration: {:?}", names(&idx, &defs));
        assert!(defs.iter().all(|&d| file_of(d) == file), "{what} -> {:?}", defs.iter().map(|&d| file_of(d)).collect::<Vec<_>>());
    }

    // Clicking the forward declaration itself jumps to the definition too.
    let fwd = text.find("class Window;").unwrap() as u32 + 6;
    let defs = q.definition(&p, &text, fwd);
    assert!(defs.iter().all(|&d| is_def(d) && file_of(d) == "window.h"), "{:?}", names(&idx, &defs));
    let ns = text.find("class Mesh;").unwrap() as u32 + 6;
    let defs = q.definition(&p, &text, ns);
    assert!(defs.iter().all(|&d| is_def(d) && file_of(d) == "mesh.h"), "{:?}", names(&idx, &defs));

    // With no definition anywhere, the forward declaration is the answer.
    let only = std::fs::read_to_string(ws.path("only.cpp")).unwrap();
    let defs = q.definition(&ws.path("only.cpp"), &only, only.find("Lonely*").unwrap() as u32);
    assert_eq!(names(&idx, &defs), ["Lonely"]);
}

#[test]
fn variable_template_with_default_bool_param_does_not_leak_symbols() {
    let src = "#pragma warning(push)\n#pragma warning(disable : 4806) // no value\n\
               template <class _Elem1, class _Elem2,\n    bool = sizeof(_Elem1) == sizeof(_Elem2) && is_integral_v<_Elem1> && is_integral_v<_Elem2>>\n\
               constexpr bool _Can_memcmp_elements =\n    is_same_v<_Elem1, bool> || is_same_v<_Elem2, bool> || static_cast<_Elem1>(-1) == static_cast<_Elem2>(-1);\n\
               #pragma warning(pop)\nstruct After { int x; };\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let all: Vec<String> = idx.live_files().flat_map(|(f, _)| idx.file_symbols(f)).map(|s| format!("{:?} {}", idx.db.symbol(s).kind, idx.db.qualified_name(s))).collect();
    assert!(idx.db.lookup("static_cast").is_empty(), "{all:?}");
    assert_eq!(idx.db.lookup("_Can_memcmp_elements").len(), 1, "{all:?}");
    assert_eq!(idx.db.lookup("After").len(), 1, "{all:?}");
}

#[test]
fn unqualified_names_do_not_match_members_of_unrelated_scopes() {
    let src = "namespace Dummy { struct Test { unsigned long u64; }; }\n\
               namespace t { using u64 = unsigned long; enum Plain { Red }; enum class Strong { Blue }; }\n\
               namespace other { void helper(int); }\n\
               namespace { struct Hidden { int h; }; }\n\
               namespace t { void inside() { u64 a; } }\n\
               struct Outer { using Id = int; Id make(); struct Inner { Id x; }; };\n\
               int test_func() { u64 vv; Red; Hidden h; helper(1); }\n\
               void other_fn() { using namespace t; u64 w; Red; }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |s: &str, nth: usize| src.match_indices(s).nth(nth).unwrap().0 as u32;
    let func = at("int test_func", 0) as usize;
    let in_func = |s: &str| (func + src[func..].find(s).unwrap()) as u32;

    // Neither `Dummy::Test::u64` nor `t::u64` is visible from the global function.
    assert!(names(&idx, &q.definition(&p, src, in_func("u64 vv"))).is_empty());
    // Unscoped enumerators of a namespace aren't visible either; anonymous namespaces are.
    assert!(names(&idx, &q.definition(&p, src, in_func("Red"))).is_empty());
    assert_eq!(names(&idx, &q.definition(&p, src, in_func("Hidden"))).len(), 1);
    // Function calls stay lenient (argument-dependent lookup finds them).
    assert_eq!(names(&idx, &q.definition(&p, src, in_func("helper"))), ["other::helper"]);

    // Inside the namespace, and via `using namespace`, they resolve.
    assert_eq!(names(&idx, &q.definition(&p, src, at("u64 a", 0))), ["t::u64"]);
    let other = at("void other_fn", 0) as usize;
    assert_eq!(names(&idx, &q.definition(&p, src, (other + src[other..].find("u64 w").unwrap()) as u32)), ["t::u64"]);
    assert_eq!(names(&idx, &q.definition(&p, src, (other + src[other..].rfind("Red").unwrap()) as u32)), ["t::Plain::Red"]);
    // Declaration sites resolve to themselves, and class-scope names resolve in nested scopes.
    assert_eq!(names(&idx, &q.definition(&p, src, at("u64;", 0))), ["Dummy::Test::u64"]);
    assert_eq!(names(&idx, &q.definition(&p, src, at("Id x", 0))), ["Outer::Id"]);
}

#[test]
fn std_containers_substitute_template_arguments() {
    let std_h = "#pragma once\nnamespace std {\n\
        template<class A, class B> struct pair { A first; B second; };\n\
        template<class T> struct allocator {};\n\
        template<class T, class Alloc = allocator<T>> class vector {\n\
        public:\n typedef T value_type;\n typedef value_type& reference;\n typedef T* iterator;\n\
         reference operator[](unsigned i);\n T& front();\n value_type& back();\n iterator begin();\n iterator end();\n void push_back(const T& v);\n};\n\
        template<class K, class V, class H = int> class unordered_map {\n\
        public:\n using iterator = pair<const K, V>*;\n V& operator[](const K& k);\n iterator find(const K& k);\n};\n}\n";
    let main = "#include \"std.h\"\nstruct Foo { int x; void run(); };\nstruct Bar { void go(); };\nstruct Other { void run(); void go(); };\n\
        void f(std::vector<Foo>& v, std::unordered_map<int, Foo> m, std::unordered_map<int, std::vector<Bar>> nested) {\n\
        v.front().run();\n v.back().run();\n v[0].run();\n m[1].run();\n m.find(1)->second.run();\n m.find(1)->first;\n\
        for (auto& e : v) e.run();\n for (auto& kv : m) kv.second.run();\n\
        auto it = v.begin(); it->run();\n nested[3][0].go();\n auto& r = m[2]; r.run();\n}\n";
    let ws = Ws::new(&[("std.h", std_h), ("a.cpp", main)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let find = |stmt: &str, member: &str| {
        let s = main.find(stmt).unwrap_or_else(|| panic!("{stmt}"));
        (s + stmt.rfind(member).unwrap()) as u32
    };
    let cases = [
        ("v.front().run()", "run", "Foo::run"),
        ("v.back().run()", "run", "Foo::run"),
        ("v[0].run()", "run", "Foo::run"),
        ("m[1].run()", "run", "Foo::run"),
        ("m.find(1)->second.run()", "run", "Foo::run"),
        ("e.run()", "run", "Foo::run"),
        ("kv.second.run()", "run", "Foo::run"),
        ("it->run()", "run", "Foo::run"),
        ("nested[3][0].go()", "go", "Bar::go"),
        ("r.run()", "run", "Foo::run"),
    ];
    for (stmt, member, want) in cases {
        let got = names(&idx, &q.definition(&p, main, find(stmt, member)));
        assert_eq!(got, [want], "{stmt}");
    }
}

#[test]
fn template_parameters_are_local_names() {
    let src = "struct T {};\ntemplate<class T, int N = 3> struct Box {\n T v; T get(T d) { return d; }\n int arr[N];\n};\n\
               template<typename U> U id(U x) { U y = x; return y; }\n\
               template<class T> struct Other { T w; };\nT global;\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |s: &str, nth: usize| src.match_indices(s).nth(nth).unwrap().0 as u32;
    // Box's T: header, `T v`, `T get`, `T d` = 4 uses; Other's T and the struct T are separate.
    let l = q.local_at(&p, src, at("T v", 0)).unwrap();
    assert_eq!(l.uses.len(), 4);
    assert!(q.local_at(&p, src, at("T v", 0)).is_some());
    assert_eq!(q.local_at(&p, src, at("U y", 0)).unwrap().uses.len(), 4);
    assert_eq!(q.local_at(&p, src, at("N]", 0)).unwrap().uses.len(), 2);
    assert_eq!(q.local_at(&p, src, at("T w", 0)).unwrap().uses.len(), 2);
    assert!(q.local_at(&p, src, at("T global", 0)).is_none());
    assert_eq!(q.hover(&p, src, at("T v", 0)).unwrap().kind, "template parameter");
}

#[test]
fn specializations_resolve_to_matching_template() {
    let src = "template<class T> struct X { void gen(); };\n\
               template<> struct X<int> { void ints(); };\n\
               template<class T> struct X<T*> { T deref(); };\n\
               struct Foo { void run(); };\n\
               void f(X<int> a, X<Foo*> b, X<float> c) { a.ints(); b.deref().run(); c.gen(); X<int> z; X<float> w; }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |s: &str| src.find(s).unwrap() as u32;
    let d = q.definition(&p, src, at("X<int> z"));
    assert_eq!(d.len(), 1);
    assert_eq!(idx.db.symbol(d[0]).name_offset, src.find("X<int> {").unwrap() as u32);
    let d = q.definition(&p, src, at("X<float> w"));
    assert_eq!(d.len(), 1);
    assert_eq!(idx.db.symbol(d[0]).name_offset, src.find("X {").unwrap() as u32);
    assert_eq!(names(&idx, &q.definition(&p, src, at("ints()"))), ["X::ints"]);
    assert_eq!(names(&idx, &q.definition(&p, src, at("c.gen") + 2)), ["X::gen"]);
    assert_eq!(names(&idx, &q.definition(&p, src, at("run(); c"))), ["Foo::run"]);
}

#[test]
fn structured_bindings_are_locals_with_inferred_types() {
    let std_h = "#pragma once\nnamespace std {\n\
         template<class A, class B> struct pair { A first; B second; };\n\
         template<class K, class V> class map { public: using iterator = pair<const K, V>*; iterator begin(); iterator end(); };\n}\n";
    let main = "#include \"std.h\"\nstruct Foo { void run(); };\nstruct Bar { void go(); };\nstruct Both { Foo f; Bar b; };\n\
         std::pair<int, Foo> mk();\nBoth both();\n\
         void f(std::map<int, Foo>& m) {\n for (auto& [k, v] : m) { v.run(); k; }\n auto [a, b] = mk();\n b.run();\n auto [p, q] = both();\n q.go();\n p.run();\n}\n";
    let ws = Ws::new(&[("std.h", std_h), ("a.cpp", main)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |s: &str, off: usize| (main.find(s).unwrap() + off) as u32;
    for (stmt, off, want) in [("v.run()", 2, "Foo::run"), ("b.run()", 2, "Foo::run"), ("q.go()", 2, "Bar::go"), ("p.run()", 2, "Foo::run")] {
        assert_eq!(names(&idx, &q.definition(&p, main, at(stmt, off))), [want], "{stmt}");
    }
    assert_eq!(q.local_at(&p, main, at("[k, v]", 1)).unwrap().uses.len(), 2);
    assert_eq!(q.local_at(&p, main, at("v.run", 0)).unwrap().uses.len(), 2);
}

/// Real MSVC STL headers (skipped when no Visual Studio is installed).
#[test]
fn real_msvc_stl_containers() {
    let Some(inc) = std::fs::read_dir(r"C:\Program Files\Microsoft Visual Studio")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|v| std::fs::read_dir(v.path()).into_iter().flatten().flatten())
        .flat_map(|e| std::fs::read_dir(e.path().join(r"VC\Tools\MSVC")).into_iter().flatten().flatten())
        .map(|m| m.path().join("include"))
        .find(|p| p.join("vector").exists() && p.join("unordered_map").exists())
    else {
        return;
    };
    let inc = inc.to_str().unwrap().replace('\\', "/");
    let src = "#include <vector>\n#include <unordered_map>\nstruct Foo { void run(); };\n\
        void f(std::vector<Foo>& v, std::unordered_map<int, Foo>& m) {\n v.front().run();\n v[0].run();\n m[1].run();\n\
        for (auto& [k, e] : m) e.run();\n for (auto& e : v) e.run();\n auto it = m.find(1); it->second.run();\n}\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let cdb = CompileDb::from_json(&format!(
        r#"[{{"directory":"{r}","file":"a.cpp","arguments":["clang++","-c","a.cpp","-I","{inc}","-D_MSC_VER=1944","-D_WIN64","-D_M_X64","-D_CPPLIB_VER=650"]}}]"#,
        r = ws.root
    ))
    .unwrap();
    let idx = Index::build(cdb, IndexConfig::default());
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let mut failed = vec![];
    for stmt in ["v.front().run()", "v[0].run()", "m[1].run()", "e.run()", "it->second.run()"] {
        let off = (src.find(stmt).unwrap() + stmt.rfind("run").unwrap()) as u32;
        if names(&idx, &q.definition(&p, src, off)) != ["Foo::run"] {
            failed.push(stmt);
        }
    }
    eprintln!("symbols: {}", idx.db.len());
    assert!(failed.is_empty(), "unresolved with real headers: {failed:?}");
}

#[test]
fn alias_targets_resolve_in_their_enclosing_scope() {
    let src = "namespace a { struct Impl { void one(); }; using Alias = Impl; }\n\
               namespace b { struct Impl { void two(); }; using Alias = Impl; }\n\
               void f() { a::Alias x; x.one(); b::Alias y; y.two(); }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let at = |s: &str| (src.find(s).unwrap() + 2) as u32;
    assert_eq!(names(&idx, &q.definition(&p, src, at("x.one"))), ["a::Impl::one"]);
    assert_eq!(names(&idx, &q.definition(&p, src, at("y.two"))), ["b::Impl::two"]);
}

#[test]
fn auto_from_container_method_returns_substituted_element() {
    let ev = "#pragma once\nnamespace eastl {\n template<class T> struct allocator {};\n\
        template<class T, class Alloc = allocator<T>> class vector {\n public:\n typedef T value_type;\n typedef T* pointer;\n typedef value_type& reference;\n typedef const value_type& const_reference;\n\
         reference push_back();\n void push_back(const value_type& v);\n void push_back(value_type&& v);\n reference emplace_back();\n\
         reference pushBack();\n T& popBack();\n reference at(unsigned i);\n};\n}\n";
    let main = "#include \"ev.h\"\nstruct SomeClass { void memberFunction(); };\nstruct Other { void memberFunction(); };\n\
        void f(eastl::vector<SomeClass>& vector, eastl::vector<SomeClass> v2) {\n\
         auto x = vector.pushBack();\n x.memberFunction();\n\
         auto& y = v2.push_back();\n y.memberFunction();\n\
         auto z = vector.emplace_back();\n z.memberFunction();\n\
         auto w = v2.popBack();\n w.memberFunction();\n\
         auto a = vector.at(0);\n a.memberFunction();\n}\n";
    let ws = Ws::new(&[("ev.h", ev), ("a.cpp", main)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    for v in ["x", "y", "z", "w", "a"] {
        let stmt = format!("{v}.memberFunction");
        let off = (main.find(&stmt).unwrap() + 3) as u32;
        assert_eq!(names(&idx, &q.definition(&p, main, off)), ["SomeClass::memberFunction"], "{v}");
    }
}

#[test]
fn free_function_overload_chosen_by_argument_type() {
    let main = "struct Guid {};\nstruct Name {};\nstruct SubGuid : Guid {};\nstruct SA { void memberA(); };\nstruct SB { void memberB(); };\nstruct SC { void memberC(); };\n\
         SA toString(const Name& n);\nSB toString(const Guid& g);\nSC toString(int a, int b);\nSA toString(float f);\n\
         void f(Guid guid, Name name, SubGuid sub) {\n\
          auto x = toString(guid);\n x.memberB();\n\
          auto y = toString(name);\n y.memberA();\n\
          auto z = toString(sub);\n z.memberB();\n\
          auto w = toString(1, 2);\n w.memberC();\n}\n";
    let ws = Ws::new(&[("a.cpp", main)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    for (v, want) in [("x", "SB::memberB"), ("y", "SA::memberA"), ("z", "SB::memberB"), ("w", "SC::memberC")] {
        let stmt = format!("{v}.member");
        let off = (main.find(&stmt).unwrap() + 3) as u32;
        assert_eq!(names(&idx, &q.definition(&p, main, off)), [want], "{v}");
    }
}

#[test]
fn eastl_style_hashtable_iterators_and_compiler_gated_aliases() {
    let h = "#pragma once\n#if defined(__clang__) || defined(_MSC_VER) || defined(__GNUC__)\n#define HAS_ALIASES 1\n#endif\n\
        namespace eastl {\n template<bool B, class T, class F> struct conditional { typedef T type; };\n\
         template<class A, class B> struct pair { A first; B second; };\n\
         template<class V, bool C> struct hit { typedef typename conditional<C, const V*, V*>::type pointer;\n pointer operator->() const; };\n\
         template<class K, class V> class hashtable { public: typedef pair<const K, V> value_type; typedef hit<value_type, false> iterator;\n iterator find(const K& k); };\n\
         template<class K, class V> class hash_map : public hashtable<K, V> {};\n\
         #if defined(HAS_ALIASES)\n template<class K, class V> using unordered_map = hash_map<K, V>;\n #endif\n}\n";
    let main = "#include \"h.h\"\nstruct Thing { void memberFunction(); };\nstruct Other { void memberFunction(); };\n\
        void f(eastl::unordered_map<int, Thing>& m) {\n auto p = m.find(1);\n p->second.memberFunction();\n}\n";
    let ws = Ws::new(&[("h.h", h), ("a.cpp", main)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let off = (main.find("second.memberFunction").unwrap() + 8) as u32;
    assert_eq!(names(&idx, &q.definition(&ws.path("a.cpp"), main, off)), ["Thing::memberFunction"]);
}

#[test]
fn auto_from_container_variants() {
    let ev = "#pragma once\nnamespace eastl {\n template<class T> struct allocator {};\n\
        template<class T, class Alloc = allocator<T>> class vector {\n public:\n typedef T value_type;\n typedef value_type& reference;\n typedef T* iterator;\n\
         reference pushBack();\n iterator begin();\n T& front();\n};\n\
        template<class T, unsigned N, bool O = true, class A = allocator<T>> class fixed_vector : public vector<T, A> {\n public:\n};\n\
        template<class T> using small_vec = vector<T>;\n}\n";
    let main = "#include \"ev.h\"\nnamespace game { struct SomeClass { void memberFunction(); }; }\nstruct Other { void memberFunction(); };\n\
        using namespace game;\nusing Vec = eastl::vector<game::SomeClass>;\ntypedef eastl::vector<SomeClass> Vec2;\n\
        struct Holder { eastl::vector<SomeClass> items; Vec alias; eastl::fixed_vector<SomeClass, 8> fixed; eastl::small_vec<SomeClass> sv; void run(); };\n\
        void Holder::run() {\n\
         auto m1 = items.pushBack();\n m1.memberFunction();\n\
         auto m2 = this->items.pushBack();\n m2.memberFunction();\n\
         auto m3 = alias.pushBack();\n m3.memberFunction();\n\
         auto m4 = fixed.pushBack();\n m4.memberFunction();\n\
         auto m5 = sv.pushBack();\n m5.memberFunction();\n\
         auto m6 = items.begin();\n m6->memberFunction();\n\
         auto m7 = items.front();\n m7.memberFunction();\n\
         auto& m8 = items.front();\n m8.memberFunction();\n\
         const auto& m9 = items.pushBack();\n m9.memberFunction();\n}\n\
        void g(Vec2& v, eastl::vector<game::SomeClass>* p, const Vec& cv) {\n\
         auto n1 = v.pushBack();\n n1.memberFunction();\n\
         auto n2 = p->pushBack();\n n2.memberFunction();\n\
         auto n3 = cv.pushBack();\n n3.memberFunction();\n\
         eastl::vector<SomeClass> loc;\n auto n4 = loc.pushBack();\n n4.memberFunction();\n\
         auto n5 = eastl::vector<SomeClass>().pushBack();\n n5.memberFunction();\n}\n";
    let ws = Ws::new(&[("ev.h", ev), ("a.cpp", main)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let mut bad = vec![];
    for v in ["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9", "n1", "n2", "n3", "n4", "n5"] {
        let stmt = format!("{v}.memberFunction");
        let stmt2 = format!("{v}->memberFunction");
        let at = main.find(&stmt).or_else(|| main.find(&stmt2)).unwrap();
        let got = names(&idx, &q.definition(&p, main, (at + 4) as u32));
        if got != ["game::SomeClass::memberFunction"] {
            bad.push(format!("{v}: {got:?}"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn alias_cycles_with_many_same_named_aliases_terminate() {
    let mut src = String::new();
    for i in 0..300 {
        src.push_str(&format!("namespace n{i} {{ using A = B; using B = A; }}\n"));
    }
    src.push_str("struct Foo { void run(); };\nvoid f() { A a; a.run(); Foo g; g.run(); }\n");
    let ws = Ws::new(&[("a.cpp", &src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let t = std::time::Instant::now();
    let off = (src.find("a.run").unwrap() + 2) as u32;
    let _ = q.definition(&p, &src, off);
    assert_eq!(names(&idx, &q.definition(&p, &src, (src.find("g.run").unwrap() + 2) as u32)), ["Foo::run"]);
    assert!(t.elapsed().as_secs() < 5, "alias resolution blew up");
}

#[test]
fn reserved_words_are_never_symbols() {
    let src = "struct static_cast_t {};\nint go(long x) { int y = static_cast<int>(x); return sizeof(y) + (this ? 1 : 0); }\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    for w in ["static_cast<", "int y", "return", "sizeof", "long x"] {
        let off = src.find(w).unwrap() as u32;
        assert!(q.definition(&p, src, off).is_empty(), "{w}");
        assert!(q.hover(&p, src, off).is_none(), "{w}");
    }
    assert_eq!(names(&idx, &q.definition(&p, src, src.find("static_cast_t").unwrap() as u32)), ["static_cast_t"]);
}

#[test]
fn using_directives_and_aliases_from_headers_and_namespace_aliases() {
    let types = "#pragma once\nnamespace gfx { using u64 = unsigned long; struct Mesh {}; }\nnamespace fs = gfx;\n\
                 inline void f() { using namespace other; }\nnamespace other { struct Leak {}; }\n";
    let hdr = "#pragma once\n#include \"types.h\"\nusing namespace gfx;\n";
    let main = "#include \"hdr.h\"\nvoid g() { u64 a; Mesh m; fs::Mesh n; Leak l; }\n";
    let ws = Ws::new(&[("types.h", types), ("hdr.h", hdr), ("a.cpp", main)], &[]);
    let mut idx = ws.index(&["a.cpp"]);
    let p = ws.path("a.cpp");
    {
        let q = Query::new(&idx, &DiskText);
        let at = |s: &str| main.find(s).unwrap() as u32;
        assert_eq!(names(&idx, &q.definition(&p, main, at("u64 a"))), ["gfx::u64"]);
        assert_eq!(names(&idx, &q.definition(&p, main, at("Mesh m"))), ["gfx::Mesh"]);
        assert_eq!(names(&idx, &q.definition(&p, main, at("Mesh n"))), ["gfx::Mesh"]);
        // A using inside a function body in a header doesn't leak into includers.
        assert!(names(&idx, &q.definition(&p, main, at("Leak"))).is_empty());
    }
    // Removing the using from the header must invalidate the cached closure.
    ws.write("hdr.h", "#pragma once\n#include \"types.h\"\n");
    idx.update(&ws.path("hdr.h"), None);
    idx.update(&p, None);
    idx.flush();
    let q = Query::new(&idx, &DiskText);
    let at = main.find("u64 a").unwrap() as u32;
    assert!(names(&idx, &q.definition(&p, main, at)).is_empty());
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
fn symbol_search_matches_terms_in_any_order() {
    let src = "namespace gfx { struct Renderer { void draw_mesh(); void keyword(); }; }\n\
void key_word(); void KeyWord(); void word_key(); void keyboard(); void unrelated();\n\
struct RigidBody { int mass; };\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let found = |s: &str| q.search_symbols(s, 20).iter().map(|&i| idx.db.qualified_name(i)).collect::<Vec<_>>();

    // Joined spellings rank first, then the same words in other orders / positions.
    let r = found("key word");
    assert_eq!(r.len(), 4, "{r:?}");
    assert!(r[..3].iter().all(|n| n.ends_with("keyword") || n.ends_with("key_word") || n.ends_with("KeyWord")), "{r:?}");
    assert_eq!(r[3], "word_key", "{r:?}");
    assert_eq!(found("word key")[0], "word_key");

    // Case-insensitive, any order, container terms.
    assert_eq!(found("MESH draw"), ["gfx::Renderer::draw_mesh"]);
    assert_eq!(found("renderer mesh"), ["gfx::Renderer::draw_mesh"]);
    assert_eq!(found("gfx keyword"), ["gfx::Renderer::keyword"]);
    assert!(found("mesh nothing").is_empty() && found("   ").is_empty());
    assert_eq!(found("rigid"), ["RigidBody"]);
    assert_eq!(q.search_symbols("key", 2).len(), 2);
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
