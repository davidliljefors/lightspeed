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

/// What is stored for the identifier right after the `/*@*/` marker in `a.cpp`.
fn marked(files: &[(&str, &str)], src: &str) -> Vec<String> {
    let at = src.find("/*@*/").expect("marker");
    let clean = src.replacen("/*@*/", "", 1);
    let len = clean[at..].find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(clean.len() - at);
    let mut all: Vec<(&str, &str)> = files.to_vec();
    all.push(("a.cpp", &clean));
    let ws = Ws::new(&all, &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let path = ws.path("a.cpp");
    let (occ, _) = q.build_occurrences();
    let fi = idx.live_files().find(|(_, f)| f.path == path).map(|(i, _)| i).unwrap();
    occ[fi]
        .sites
        .iter()
        .filter(|(o, _)| (*o as usize) >= at && (*o as usize) < at + len)
        .map(|&(_, s)| idx.db.qualified_name(s))
        .collect()
}

#[test]
fn member_of_an_anonymous_struct_typedef_is_not_another_structs_member() {
    // zstd: `typedef struct { ... seqStore; } Ldm;` and an unrelated `Ctx::seqStore`.
    let got = marked(&[], "struct Ctx { int store; };\ntypedef struct { int store; } Ldm;\nint f(Ldm* p) { return p->/*@*/store; }\n");
    assert!(!got.contains(&"Ctx::store".to_string()), "{got:?}");
}

#[test]
fn member_of_a_function_local_struct_is_not_the_only_indexed_member_of_that_name() {
    // Jolt: `struct MyCollector { BodyID mGroundBodyID; } collector;` inside a function.
    let got = marked(&[], "struct Base { int id; };\nint f() {\n  struct Local { int id; };\n  Local c;\n  return c./*@*/id;\n}\n");
    assert!(!got.contains(&"Base::id".to_string()), "{got:?}");
}

#[test]
fn member_through_an_inherited_result_type_is_not_a_same_named_member_elsewhere() {
    // Jolt: `RayCastResult hit; hit.mBodyID` where the member comes from a base struct.
    let got = marked(&[], "struct CastResult { int body; };\nstruct RayResult : CastResult { float f; };\nstruct Transformed { int body; };\nvoid f() { RayResult hit; hit./*@*/body = 1; }\n");
    assert!(!got.contains(&"Transformed::body".to_string()), "{got:?}");
}

#[test]
fn member_of_a_class_member_pointer_resolves_through_the_hierarchy() {
    // Godot: `ItemList* list = nullptr;` then `list->connect(..)` is `Node::connect`, not `Signal::connect`.
    let got = marked(
        &[],
        "struct Signal { int connect(int); };\nstruct Object { virtual int connect(int); };\nstruct Node : Object { int connect(int) override; };\nstruct ItemList : Node {};\nstruct Editor { ItemList* list = nullptr; void build(); };\nvoid Editor::build() { list->/*@*/connect(1); }\n",
    );
    assert!(!got.contains(&"Signal::connect".to_string()), "{got:?}");
}

#[test]
fn macro_argument_is_not_a_use_of_the_same_named_method() {
    // Godot: `SceneStringName(toggled)` expands to `Names::get()->toggled`, not `Button::toggled`.
    let got = marked(
        &[],
        "struct Names { int toggled; static Names* get(); };\n#define NAME(m) Names::get()->m\nstruct Button { void toggled(); };\nint f() { return NAME(/*@*/toggled); }\n",
    );
    assert!(!got.contains(&"Button::toggled".to_string()), "{got:?}");
}

#[test]
fn using_namespace_picks_the_namespace_not_a_same_named_class_elsewhere() {
    // Godot: `using namespace RendererDummy; LightStorage* LightStorage::singleton`.
    let got = marked(
        &[("rd.h", "namespace RD { struct Store { static Store* singleton; }; }\n"), ("dummy.h", "namespace Dummy { struct Store { static Store* singleton; }; }\n")],
        "#include \"rd.h\"\n#include \"dummy.h\"\nusing namespace Dummy;\nStore* Store::singleton = nullptr;\nvoid f() { /*@*/Store* s = nullptr; }\n",
    );
    assert!(!got.contains(&"RD::Store".to_string()), "{got:?}");
}

#[test]
fn qualified_nested_type_of_a_class_with_an_export_macro_is_not_another_classes_nested_type() {
    // Jolt: `class JPH_EXPORT ConvexHullBuilder { class Face; }` and an unrelated `ConvexHullShape::Face`.
    let got = marked(
        &[("b.h", "#define EXPORT __attribute__ ((visibility (\"default\")))\nclass EXPORT Builder : public Base {\npublic:\n  class Face;\n  class Face : public Base {};\n};\n"), ("o.h", "struct Other { struct Face {}; };\n")],
        "#include \"b.h\"\n#include \"o.h\"\nvoid f() { Builder::/*@*/Face* p = nullptr; }\n",
    );
    assert!(!got.contains(&"Other::Face".to_string()), "{got:?}");
}

#[test]
fn bare_name_in_a_function_local_struct_is_not_an_outer_members() {
    // Jolt: a collector struct declared inside a member function, with a field of the outer class's name.
    let got = marked(&[], "struct Base { int id; void f(); };\nvoid Base::f() {\n  struct L { int id; void set() { /*@*/id = 1; } };\n}\n");
    assert!(!got.contains(&"Base::id".to_string()), "{got:?}");
}

#[test]
fn constructor_initializer_names_the_field() {
    let got = marked(&[], "struct P { int plural; P(int v); };\nP::P(int v) : /*@*/plural(v) {}\n");
    assert_eq!(got, ["P::plural"]);
}

#[test]
fn class_with_an_empty_export_macro_is_still_a_class() {
    let got = marked(
        &[("b.h", "#define EXPORT\nclass EXPORT Builder { public: class Face; };\n"), ("o.h", "struct Other { struct Face {}; };\n")],
        "#include \"b.h\"\n#include \"o.h\"\nvoid f() { /*@*/Builder* b = nullptr; }\n",
    );
    assert_eq!(got, ["Builder"]);
}

#[test]
fn namespace_opened_after_msvc_pragma_operators_keeps_its_scope() {
    // Jolt on MSVC: `#define NS_BEGIN __pragma(warning(push)) namespace N {`.
    let got = marked(
        &[("b.h", "#define PUSH __pragma(warning (push))\n#define NS_BEGIN PUSH __pragma(warning (disable : 4100)) namespace N {\nNS_BEGIN\nclass Builder { public: class Face; };\n}\n")],
        "#include \"b.h\"\nvoid f() { N::/*@*/Builder* b = nullptr; }\n",
    );
    assert_eq!(got, ["N::Builder"]);
}

#[test]
fn namespace_opened_after_an_indirect_pragma_operator_keeps_its_scope() {
    let got = marked(
        &[("b.h", "#define PRAGMA(x) __pragma(x)\n#define PUSH PRAGMA(warning (push))\nPUSH\nnamespace N {\nclass Builder {};\n}\n")],
        "#include \"b.h\"\nvoid f() { N::/*@*/Builder* b = nullptr; }\n",
    );
    assert_eq!(got, ["N::Builder"]);
}

#[test]
fn namespace_opened_after_macros_that_expand_to_nothing_keeps_its_scope() {
    let got = marked(
        &[("b.h", "#define SUP(w)\n#define NOTHING\n#define IFN(x)\n#define ALL SUP(\"-Wa\") SUP(\"-Wb\") NOTHING IFN(SUP(\"-Wc\")) SUP(\"-Wd\")\n#define NS_BEGIN ALL namespace N {\nNS_BEGIN\nclass Builder { public: class Face; };\n}\n")],
        "#include \"b.h\"\nvoid f() { N::/*@*/Builder* b = nullptr; }\n",
    );
    assert_eq!(got, ["N::Builder"]);
}

#[test]
fn namespace_opened_after_a_macro_with_commented_continuation_lines() {
    let got = marked(
        &[("b.h", "#define SUP(w)\n#define ALL \\\n\tSUP(1) /* 'X' : unreferenced formal parameter */\t\t\\\n\tSUP(2) /* class 'X' needs to have DLL-interface to 'Y' */ \\\n\tSUP(3) /* last 'X': one */\n#define NS_BEGIN ALL namespace N {\nNS_BEGIN\nclass Builder { public: class Face; };\n}\n")],
        "#include \"b.h\"\nvoid f() { N::/*@*/Builder* b = nullptr; }\n",
    );
    assert_eq!(got, ["N::Builder"]);
}

#[test]
fn namespace_opened_after_parameterised_pragma_macros_keeps_its_scope() {
    let got = marked(
        &[("b.h", "#define PRAGMA(x) __pragma(x)\n#define MSVC(w) PRAGMA(warning (disable : w))\n#define ALL MSVC(4619) MSVC(4514)\n#define NS_BEGIN ALL namespace N {\nNS_BEGIN\nclass Builder {};\n}\n")],
        "#include \"b.h\"\nvoid f() { N::/*@*/Builder* b = nullptr; }\n",
    );
    assert_eq!(got, ["N::Builder"]);
}

#[test]
fn namespace_opened_after_stringified_pragma_operators_keeps_its_scope() {
    let got = marked(
        &[("b.h", "#define PRAGMA(x) _Pragma(#x)\n#define SUP(w) PRAGMA(clang diagnostic ignored w)\n#define ALL PRAGMA(clang diagnostic push) SUP(\"-Wfoo\") SUP(\"-Wbar\")\n#define NS_BEGIN ALL namespace N {\nNS_BEGIN\nclass Builder {};\n}\n")],
        "#include \"b.h\"\nvoid f() { N::/*@*/Builder* b = nullptr; }\n",
    );
    assert_eq!(got, ["N::Builder"]);
}

#[test]
fn qualifier_naming_nothing_we_index_does_not_pick_a_same_named_stranger() {
    let got = marked(
        &[("a.h", "namespace Other { struct Face {}; }\n")],
        "#include \"a.h\"\nvoid f() { Unknown::/*@*/Face* x = nullptr; }\n",
    );
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn local_class_field_declared_after_use_is_not_an_inherited_outer_member() {
    let got = marked(
        &[],
        "struct Base { int id; };\nstruct Coll { virtual void hit(); };\nstruct D : Base { void f(); };\nvoid D::f() {\n  class L : public Coll {\n  public:\n    virtual void hit() override { /*@*/id = 1; }\n    int id;\n  };\n}\n",
    );
    assert!(!got.contains(&"Base::id".to_string()), "{got:?}");
}

#[test]
fn member_qualified_by_a_template_id_is_not_the_enclosing_classes_member() {
    // embree: `vfloat<M>::store_nt` inside a class that has its own `store_nt`.
    let got = marked(
        &[],
        "template<class T> struct V { static void store(int); };\ntemplate<int M> struct Tri { void store(int); void f() { V<M>::/*@*/store(1); } };\n",
    );
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn alias_to_a_namespaced_template_does_not_resolve_to_a_global_class_of_that_name() {
    // Godot's `Array` vs `JPH::Array`, reached through `JPH::TriangleList`.
    let got = marked(
        &[("g.h", "struct Array { void reserve(int); };\n"), ("j.h", "namespace JPH { template<class T> struct Array { void reserve(int); }; using List = Array<int>; }\n")],
        "#include \"g.h\"\n#include \"j.h\"\nvoid f() { JPH::List l; l./*@*/reserve(1); }\n",
    );
    assert!(!got.contains(&"Array::reserve".to_string()), "{got:?}");
}

#[test]
fn class_with_an_attribute_between_keyword_and_name_is_a_class() {
    let got = marked(
        &[("b.h", "namespace N { class [[nodiscard]] Builder { public: struct Face {}; }; }\n"), ("o.h", "namespace M { struct Face {}; }\n")],
        "#include \"b.h\"\n#include \"o.h\"\nvoid f() { N::Builder::/*@*/Face* x; }\n",
    );
    assert_eq!(got, ["N::Builder::Face"]);
}

#[test]
#[ignore = "headers are parsed standalone, so macros from their includers are unknown"]
fn header_that_relies_on_its_includers_macros_never_misleads_a_same_named_global() {
    // Jolt: `Array.h` opens its namespace with a macro from `Core.h`, which it does not include.
    // Parsed standalone the class is lost; the global `Array` must not be picked in its place.
    let got = marked(
        &[
            ("m.h", "#define NS_BEGIN namespace JPH {\n#define NS_END }\n"),
            ("a.h", "#pragma once\nNS_BEGIN\ntemplate <class T> class [[nodiscard]] Array { public: void reserve(int); };\nusing List = Array<int>;\nNS_END\n"),
            ("g.h", "struct Array { void reserve(int); };\n"),
        ],
        "#include \"m.h\"\n#include \"a.h\"\n#include \"g.h\"\nvoid f() { JPH::List l; l./*@*/reserve(1); }\n",
    );
    assert!(!got.contains(&"Array::reserve".to_string()), "{got:?}");
}

#[test]
fn argument_of_an_unknown_all_caps_macro_is_not_a_use_of_the_same_named_method() {
    // Godot's `EXBIND1(set_layer, int)` comes from a generated header: it declares a method rather than using one.
    let got = marked(&[], "struct Base { virtual void set_layer(int); };\nstruct D : Base {\n  EXBIND1(/*@*/set_layer, int)\n};\n");
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn bare_call_in_an_out_of_line_member_of_a_namespaced_template_is_not_a_same_named_global_class() {
    // graphite2: `Vector<T>::resize` calls `erase`; Godot has a global `Vector` with its own `erase`.
    let global = "template <typename T> class Vector { public: void erase(int a); };\n";
    let lib = "namespace g {\ntemplate <typename T> class Vector {\npublic:\n  void resize(int n);\n  void erase(int a, int b);\n};\ntemplate <typename T> void Vector<T>::resize(int n) {\n  if (n < 0) /*@*/erase(n, n);\n}\n}\n";
    let got = marked(&[("v.h", global)], &format!("#include \"v.h\"\n{lib}"));
    assert_eq!(got, ["g::Vector::erase"], "{got:?}");
}

/// Like `marked`, but the `/*@*/` marker sits in one of the `files` (a header) and `main` is the translation unit.
fn marked_in_header(files: &[(&str, &str)], main: &str) -> Vec<String> {
    let (hname, htext) = files.iter().find(|(_, t)| t.contains("/*@*/")).expect("marker");
    let at = htext.find("/*@*/").unwrap();
    let clean = htext.replacen("/*@*/", "", 1);
    let len = clean[at..].find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(clean.len() - at);
    let mut all: Vec<(&str, &str)> = files.iter().map(|&(n, t)| if n == *hname { (n, clean.as_str()) } else { (n, t) }).collect();
    all.push(("a.cpp", main));
    let ws = Ws::new(&all, &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let path = ws.path(hname);
    let (occ, _) = q.build_occurrences();
    let fi = idx.live_files().find(|(_, f)| f.path == path).map(|(i, _)| i).unwrap();
    occ[fi].sites.iter().filter(|(o, _)| (*o as usize) >= at && (*o as usize) < at + len).map(|&(_, s)| idx.db.qualified_name(s)).collect()
}

#[test]
fn bare_call_in_a_header_out_of_line_member_of_a_namespaced_template_is_not_a_same_named_global_class() {
    let global = "template <typename T> class Vector { public: void erase(int a); };\n";
    let lib = "namespace g {\ntemplate <typename T> class Vector {\npublic:\n  void resize(int n);\n  void erase(int a, int b);\n};\ntemplate <typename T> void Vector<T>::resize(int n) {\n  if (n < 0) /*@*/erase(n, n);\n}\n}\n";
    let got = marked_in_header(&[("v.h", global), ("g.h", lib)], "#include \"v.h\"\n#include \"g.h\"\n");
    assert!(!got.contains(&"Vector::erase".to_string()), "{got:?}");
}

#[test]
fn out_of_line_member_goes_to_the_class_of_its_own_namespace_whichever_is_indexed_first() {
    let global = "template <typename T> class Vector { public: void erase(int a); void resize(int n); };\n";
    let lib = "namespace g {\ntemplate <typename T> class Vector {\npublic:\n  void resize(int n);\n  void erase(int a, int b);\n};\ntemplate <typename T> void Vector<T>::resize(int n) {\n  if (n < 0) /*@*/erase(n, n);\n}\n}\n";
    for main in ["#include \"v.h\"\n#include \"g.h\"\n", "#include \"g.h\"\n#include \"v.h\"\n", "#include \"g.h\"\n"] {
        let got = marked_in_header(&[("v.h", global), ("g.h", lib)], main);
        assert!(!got.contains(&"Vector::erase".to_string()), "{main:?}: {got:?}");
    }
    // The global class is only reachable from another translation unit.
    let ws_files = [("v.h", global), ("g.h", lib)];
    let got = marked_in_header(&ws_files, "#include \"g.h\"\n");
    assert!(!got.contains(&"Vector::erase".to_string()), "{got:?}");
}

#[test]
fn identifier_in_a_define_body_is_not_a_use_of_the_same_named_method() {
    // Godot's variant_utility.cpp declares `get_type` inside a macro body, a different function from Variant::get_type.
    let got = marked(&[], "struct Variant { int get_type() const; };\n#define DECL(m) \\\n  struct U { \\\n    static int /*@*/get_type() { return m; } \\\n  };\n");
    assert!(got.is_empty(), "{got:?}");
}
