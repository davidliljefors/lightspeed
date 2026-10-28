//! Hover, signature help, completion, references and symbol search.

mod common;

use common::{Ws, cursor};
use lightspeed::query::{DiskText, Query};

#[test]
fn hover_evaluates_constants_macros_and_enums() {
    let hdr = "#pragma once\nnamespace cfg { constexpr int Base = 40; }\n#define BASE_MACRO (cfg::Base + 2)\n";
    let src = "#include \"h.h\"\n\
        #define TWICE (BASE_MACRO * 2)\n#define FN(x) (x)\n\
        const int a = 5 + cfg::Base;\nstatic constexpr unsigned mask = 1u << 4 | sizeof(int);\n\
        const int cyc1 = cyc2;\nconst int cyc2 = cyc1;\nint plain = 3;\nconst double d = 1.5 * 2;\n\
        enum E { A, B = 10, C, D = B + 5, F };\n\
        struct S { static constexpr int K = a + 1; };\nconst int viaS = S::K * 2;\nconst int call = foo(1);\n\
        const int u0 = a; const int u1 = TWICE; const int u2 = mask; const int u3 = cyc1; const int u4 = plain;\n\
        const int u5 = d; const int u6 = C; const int u7 = F; const int u8 = viaS; const int u9 = call; const int u10 = FN;\n";
    let ws = Ws::new(&[("a.cpp", src), ("h.h", hdr)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let note = |name: &str| {
        let off = src.find(&format!("= {name};")).unwrap() as u32 + 2;
        q.hover(&p, src, off).and_then(|h| h.notes.into_iter().find(|n| n.starts_with("value: ")))
    };
    assert_eq!(note("a").as_deref(), Some("value: 45 (0x2D)"));
    assert_eq!(note("TWICE").as_deref(), Some("value: 84 (0x54)"));
    assert_eq!(note("mask").as_deref(), Some("value: 20 (0x14)"));
    assert_eq!(note("cyc1"), None);
    assert_eq!(note("plain"), None);
    assert_eq!(note("d").as_deref(), Some("value: 3.0"));
    assert_eq!(note("C").as_deref(), Some("value: 11"));
    assert_eq!(note("F").as_deref(), Some("value: 16 (0x10)"));
    assert_eq!(note("viaS").as_deref(), Some("value: 92 (0x5C)"));
    assert_eq!(note("call"), None);
    assert_eq!(note("FN"), None);
}

#[test]
fn signature_help_ranks_overloads_by_typed_arguments() {
    let src = "struct Guid {}; struct Name {};\n\
        void put(); void put(int a); void put(Guid g, int n); void put(Name n, int k, bool f); void put(Name n, Guid g);\n\
        struct W { W(); W(int x); W(Guid g, Name n); };\n\
        void use(Guid guid) {\n  put(\n  put(guid, \n  put(guid, 1, \n  W(\n}\n";
    let ws = Ws::new(&[("a.cpp", src)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let p = ws.path("a.cpp");
    let after = |s: &str| (src.find(s).unwrap() + s.len()) as u32;

    let h = q.signature_help(&p, src, after("  put("), 3).unwrap();
    let labels: Vec<&str> = h.signatures.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, ["put()", "put(int a)", "put(Guid g, int n)"]);
    assert_eq!((h.active_param, h.more), (0, 2));

    // A typed `Guid` first argument selects the Guid overloads, shortest first.
    let h = q.signature_help(&p, src, after("put(guid, "), 3).unwrap();
    assert_eq!(h.active_param, 1);
    assert_eq!(h.signatures[0].label, "put(Guid g, int n)");
    let (a, b) = h.signatures[0].params[1];
    assert_eq!(&h.signatures[0].label[a as usize..b as usize], "int n");

    let h = q.signature_help(&p, src, after("put(guid, 1, "), 1).unwrap();
    assert_eq!(h.active_param, 2);
    assert_eq!(h.signatures.len(), 1);

    let h = q.signature_help(&p, src, after("  W("), 2).unwrap();
    assert_eq!(h.signatures[0].label, "W()");
    assert!(q.signature_help(&p, src, after("struct Guid {}"), 3).is_none());
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
fn forward_declarations_are_not_listed_and_do_not_change_colour() {
    let hdr = "class Widget;\nstruct Gadget;\nstruct Orphan;\n";
    let src = "#include \"h.h\"\nstruct Widget { int a; };\nclass Gadget { int b; };\nWidget* w; Gadget* g; Orphan* o;\n";
    let ws = Ws::new(&[("a.cpp", src), ("h.h", hdr)], &[]);
    let idx = ws.index(&["a.cpp"]);
    let q = Query::new(&idx, &DiskText);
    let found = |s: &str| q.search_symbols(s, 20).iter().map(|&i| (idx.db.qualified_name(i), idx.db.file_path(i).ends_with("a.cpp"))).collect::<Vec<_>>();
    assert_eq!(found("Widget"), [("Widget".to_string(), true)]);
    assert_eq!(found("Gadget"), [("Gadget".to_string(), true)]);
    // Without a definition the forward declaration is all there is.
    assert_eq!(found("Orphan"), [("Orphan".to_string(), false)]);

    let toks = q.semantic_tokens(&ws.path("a.cpp"), src, None);
    let ty_at = |needle: &str, nth_hit: usize| {
        let at = src.match_indices(needle).nth(nth_hit).unwrap().0 as u32;
        toks.iter().find(|t| t.start == at).map(|t| t.ty)
    };
    // `Widget` is declared `class` in the header but defined as a struct.
    assert_eq!(ty_at("Widget", 1), ty_at("Widget", 0));
    assert_eq!(ty_at("Gadget", 1), ty_at("Gadget", 0));
    assert_ne!(ty_at("Widget", 1), ty_at("Gadget", 1), "struct vs class definitions keep their own colour");
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
