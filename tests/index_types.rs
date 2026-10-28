//! Receiver type inference: containers, `auto`, smart pointers, bindings.

mod common;

use common::{Ws, cursor, names};
use lightspeed::query::{DiskText, Query};
use lightspeed::workspace::compile_commands::CompileDb;
use lightspeed::workspace::index::{Index, IndexConfig};

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
