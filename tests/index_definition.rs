//! Go-to-definition: scope, qualifier, overload and alias resolution.

mod common;

use common::{Ws, cursor, names};
use lightspeed::query::{DiskText, Query};

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
