//! Hierarchy tests: base resolution, virtual families, out-of-line linking.

use lightspeed::workspace::compile_commands::FlagSet;
use lightspeed::hierarchy::Hierarchy;
use lightspeed::base::ids::Idx;
use lightspeed::parser::{ParseOutput, parse};
use lightspeed::pp::vfs::MemFs;
use lightspeed::pp::{PpConfig, PpOutput, Preprocessor};
use lightspeed::symbols::{SymbolDb, SymbolId};

fn shard(path: &str, src: &str) -> SymbolDb {
    let fs = MemFs::new();
    let mut pp = Preprocessor::new(&fs, PpConfig::default());
    pp.set_flags(&FlagSet::default());
    let mut out = PpOutput::default();
    pp.run(path, src, &mut out);
    let mut db = SymbolDb::new();
    let file = db.files.intern(path);
    parse(&out, src, &mut db, file, &mut ParseOutput::default());
    db
}

struct World {
    db: SymbolDb,
    h: Hierarchy,
}

fn world(files: &[&str]) -> World {
    let mut db = SymbolDb::new();
    for (i, src) in files.iter().enumerate() {
        db.merge(&shard(&format!("/t/f{i}.h"), src));
    }
    let h = Hierarchy::build(&mut db);
    World { db, h }
}

impl World {
    fn id(&self, q: &str) -> SymbolId {
        let hits: Vec<_> = (0..self.db.len()).map(SymbolId::from_usize).filter(|&i| self.db.qualified_name(i) == q).collect();
        assert!(!hits.is_empty(), "no symbol {q}");
        hits[0]
    }

    fn names(&self, ids: &[SymbolId]) -> Vec<String> {
        let mut v: Vec<String> = ids.iter().map(|&i| self.db.qualified_name(i)).collect();
        v.sort();
        v
    }

    fn family(&self, q: &str) -> Vec<String> {
        self.names(self.h.family_members(&self.db, self.id(q)))
    }
}

#[test]
fn simple_chain_with_implicit_virtual() {
    let w = world(&["struct A { virtual void f(); void g(); }; struct B : A { void f(); void g(); }; struct C : B { void f() override; };"]);
    assert_eq!(w.family("A::f"), ["A::f", "B::f", "C::f"]);
    assert_eq!(w.family("C::f"), ["A::f", "B::f", "C::f"]);
    assert!(w.family("A::g").is_empty(), "non-virtual stays out");
    assert!(w.family("B::g").is_empty());
}

#[test]
fn unrelated_same_name_is_separate() {
    let w = world(&["struct A { virtual void f(); }; struct X { virtual void f(); }; struct B : A { void f() override; };"]);
    assert_eq!(w.family("A::f"), ["A::f", "B::f"]);
    assert_eq!(w.family("X::f"), ["X::f"]);
}

#[test]
fn siblings_share_the_base_family() {
    let w = world(&["struct A { virtual void f() = 0; }; struct B : A { void f() override; }; struct C : A { void f() override; };"]);
    assert_eq!(w.family("B::f"), ["A::f", "B::f", "C::f"]);
}

#[test]
fn inherited_but_not_redeclared_skips_levels() {
    let w = world(&["struct A { virtual void f(); }; struct B : A {}; struct C : B { void f() override; };"]);
    assert_eq!(w.family("A::f"), ["A::f", "C::f"]);
}

#[test]
fn multiple_inheritance_unions_families() {
    let w = world(&["struct I1 { virtual void f(); }; struct I2 { virtual void f(); }; struct C : I1, I2 { void f() override; };"]);
    assert_eq!(w.family("I1::f"), ["C::f", "I1::f", "I2::f"]);
}

#[test]
fn diamond_inheritance_terminates() {
    let w = world(&["struct A { virtual void f(); }; struct B : virtual A { void f() override; }; struct C : virtual A { }; struct D : B, C { void f() override; };"]);
    assert_eq!(w.family("A::f"), ["A::f", "B::f", "D::f"]);
    let mut v = Vec::new();
    w.h.all_bases(w.id("D"), &mut v);
    assert_eq!(w.names(&v), ["A", "B", "C"]);
}

#[test]
fn virtual_destructors() {
    let w = world(&["struct A { virtual ~A(); }; struct B : A { ~B(); }; struct C : B { ~C() override; }; struct P { ~P(); }; struct Q : P { ~Q(); };"]);
    assert_eq!(w.family("A::~A"), ["A::~A", "B::~B", "C::~C"]);
    assert!(w.family("P::~P").is_empty());
    assert!(w.family("Q::~Q").is_empty());
}

#[test]
fn statics_and_constructors_never_join() {
    let w = world(&["struct A { virtual void f(); static void s(); A(); }; struct B : A { static void s(); B(); void f(); };"]);
    assert!(w.family("A::s").is_empty());
    assert!(w.family("B::B").is_empty());
    assert_eq!(w.family("A::f"), ["A::f", "B::f"]);
}

#[test]
fn overridden_and_overriders_queries() {
    let w = world(&["struct A { virtual void f(); }; struct B : A { void f() override; }; struct C : B { void f() override; };"]);
    let mut v = Vec::new();
    w.h.overridden(&w.db, w.id("C::f"), &mut v);
    assert_eq!(w.names(&v), ["A::f", "B::f"]);
    v.clear();
    w.h.overriders(&w.db, w.id("A::f"), &mut v);
    assert_eq!(w.names(&v), ["B::f", "C::f"]);
    v.clear();
    w.h.overriders(&w.db, w.id("C::f"), &mut v);
    assert!(v.is_empty());
}

#[test]
fn base_resolution_across_files_any_merge_order() {
    let base = "struct Base { virtual void tick(); };";
    let derived = "struct Mid : Base { void tick() override; }; struct Leaf : Mid { void tick() override; };";
    for files in [[base, derived], [derived, base]] {
        let w = world(&files);
        assert_eq!(w.family("Base::tick"), ["Base::tick", "Leaf::tick", "Mid::tick"]);
        assert_eq!(w.h.stats.bases_unresolved, 0);
        assert!(w.h.is_a(w.id("Leaf"), w.id("Base")));
        assert!(!w.h.is_a(w.id("Base"), w.id("Leaf")));
        let mut d = Vec::new();
        w.h.all_derived(w.id("Base"), &mut d);
        assert_eq!(w.names(&d), ["Leaf", "Mid"]);
        assert_eq!(w.h.derived(w.id("Base")).len(), 1);
        assert_eq!(w.h.bases(w.id("Leaf")).len(), 1);
    }
}

#[test]
fn nearest_namespace_wins_and_definition_beats_forward_decl() {
    let w = world(&["namespace a { struct B { virtual void f(); }; struct D : B { void f() override; }; } namespace b { struct B { virtual void f(); }; struct D : B { void f() override; }; }"]);
    assert_eq!(w.family("a::D::f"), ["a::B::f", "a::D::f"]);
    assert_eq!(w.family("b::D::f"), ["b::B::f", "b::D::f"]);
    let w = world(&["struct Base; struct Base { virtual void f(); }; struct D : Base { void f() override; };"]);
    assert_eq!(w.family("D::f"), ["Base::f", "D::f"]);
    let w = world(&["struct Fwd; struct D : Fwd { };", "struct Fwd { };"]);
    assert_eq!(w.h.bases(w.id("D")).len(), 1);
    assert!(w.db.symbol(w.h.bases(w.id("D"))[0]).flags.contains(lightspeed::symbols::SymbolFlags::DEFINITION));
}

#[test]
fn unresolved_bases_are_counted_not_fatal() {
    let w = world(&["struct D : Missing<int>, std::Other { virtual void f(); };"]);
    assert_eq!(w.h.stats.bases_unresolved, 2);
    assert_eq!(w.family("D::f"), ["D::f"]);
}

#[test]
fn inheritance_cycles_terminate() {
    let w = world(&["struct A : B { virtual void f(); }; struct B : C { void f(); }; struct C : A { void f(); }; struct Self : Self {};"]);
    assert!(w.h.stats.cycle_classes >= 3);
    let mut v = Vec::new();
    w.h.all_bases(w.id("A"), &mut v);
    assert_eq!(v.len(), 2);
    let _ = w.h.is_a(w.id("A"), w.id("Self"));
}

#[test]
fn out_of_line_definitions_link_to_declarations() {
    let w = world(&[
        "namespace n { struct Foo { Foo(); virtual void run(); void plain(); }; struct Bar : Foo { void run() override; }; }",
        "void n::Foo::run() {} void n::Foo::plain() {} n::Foo::Foo() {} void n::Bar::run() {} void Ghost::x() {}",
    ]);
    assert_eq!(w.h.stats.out_of_line_resolved, 4);
    assert_eq!(w.h.stats.out_of_line_unresolved, 1);
    let defs: Vec<SymbolId> = w.db.out_of_line().iter().map(|p| p.0).collect();
    let canon: Vec<String> = defs.iter().map(|&d| w.db.qualified_name(w.h.canonical(d))).collect();
    assert_eq!(canon[..4], ["n::Foo::run", "n::Foo::plain", "n::Foo::Foo", "n::Bar::run"]);
    // Definitions are family members too, so find-refs on any of them sees all.
    assert_eq!(w.family("n::Foo::run").len(), 4);
    assert_eq!(w.family("n::Foo::run"), w.names(w.h.family_members(&w.db, defs[0])));
}

#[test]
fn qualifier_must_match_whole_scope_segments() {
    let w = world(&["namespace x { struct Foo { void m(); }; } namespace y { struct Foo { void m(); }; }", "void y::Foo::m() {}"]);
    assert_eq!(w.h.canonical(w.db.out_of_line()[0].0), w.id("y::Foo::m"));
    let w = world(&["namespace xa { struct Foo { void m(); }; }", "void a::Foo::m() {}"]);
    assert_eq!(w.h.stats.out_of_line_unresolved, 1, "`a::Foo` must not match `xa::Foo`");
}

#[test]
fn out_of_line_overloads_match_by_signature_in_any_order() {
    let w = world(&[
        "struct S { void f(int); void f(float x); void f(const Foo<int, 2>&, int n = 1); void f() const; void f(); };",
        "void S::f() {} void S::f(const Foo<int, 2>& v, int) {} void S::f(float) {} void S::f() const {} void S::f(int a) {}",
    ]);
    let c: Vec<String> = w
        .db
        .out_of_line()
        .iter()
        .map(|p| {
            let m = w.h.canonical(p.0);
            let (a, b) = (w.db.signature(p.0).unwrap(), w.db.signature(m).unwrap());
            assert_eq!((a.arity, a.hash), (b.arity, b.hash), "definition and declaration signatures agree");
            format!("{}", w.db.symbol(m).name_offset)
        })
        .collect();
    let mut sorted = c.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 5, "each definition claims a distinct declaration: {c:?}");
    // Check the actual pairing by arity/kind: defs 0 and 3 are the nullary overloads.
    let m = |i: usize| w.db.symbol(w.h.canonical(w.db.out_of_line()[i].0)).flags;
    assert!(m(3).contains(lightspeed::symbols::SymbolFlags::CONST));
    assert!(!m(0).contains(lightspeed::symbols::SymbolFlags::CONST));
}

#[test]
fn rebuild_is_idempotent_and_clears_stale_families() {
    let mut w = world(&["struct A { virtual void f(); }; struct B : A { void f(); };"]);
    let before: Vec<_> = w.db.symbols().iter().map(|s| s.family).collect();
    w.h = Hierarchy::build(&mut w.db);
    let after: Vec<_> = w.db.symbols().iter().map(|s| s.family).collect();
    assert_eq!(before, after);
}

#[test]
fn partition_is_independent_of_shard_order() {
    let srcs = [
        "struct A { virtual void f(); virtual void g(); virtual ~A(); };",
        "struct B : A { void f() override; ~B(); };",
        "struct C : B { void g() override; void f(); };",
        "struct D : A { void g() override; };",
    ];
    let canon = |order: &[usize]| {
        let files: Vec<&str> = order.iter().map(|&i| srcs[i]).collect();
        let w = world(&files);
        let mut parts: Vec<Vec<String>> = (0..w.db.len())
            .map(SymbolId::from_usize)
            .filter(|&i| w.db.symbol(i).family.is_some())
            .map(|i| w.names(w.h.family_members(&w.db, i)))
            .collect();
        parts.sort();
        parts.dedup();
        parts
    };
    let a = canon(&[0, 1, 2, 3]);
    assert_eq!(a, canon(&[3, 2, 1, 0]));
    assert_eq!(a, canon(&[2, 0, 3, 1]));
    assert_eq!(a.len(), 3);
}

#[test]
fn overloads_get_separate_families() {
    let w = world(&["struct A { virtual void f(int); virtual void f(float); }; struct B : A { void f(int) override; void f(float) override; };"]);
    assert_eq!(w.family("A::f"), w.names(w.h.family_members(&w.db, w.id("A::f"))));
    let a: Vec<_> = (0..w.db.len()).map(SymbolId::from_usize).filter(|&i| w.db.name(i) == "f").collect();
    assert_eq!(a.len(), 4);
    // A::f(int) pairs with B::f(int), A::f(float) with B::f(float).
    assert_eq!(w.h.family_members(&w.db, a[0]), [a[0], a[2]]);
    assert_eq!(w.h.family_members(&w.db, a[1]), [a[1], a[3]]);
}

#[test]
fn const_and_param_names_and_defaults_do_not_confuse_overrides() {
    let w = world(&["struct A { virtual int get() const; virtual int get(); virtual void set(const Foo<int, 2>& value, int n = 3); };
                     struct B : A { int get() const override; void set(const Foo<int, 2>&, int) override; };"]);
    let ids = |n: &str| -> Vec<SymbolId> { (0..w.db.len()).map(SymbolId::from_usize).filter(|&i| w.db.name(i) == n).collect() };
    let get = ids("get");
    assert_eq!(w.h.family_members(&w.db, get[0]), [get[0], get[2]]);
    assert_eq!(w.h.family_members(&w.db, get[1]), [get[1]]);
    assert_eq!(w.family("A::set"), ["A::set", "B::set"]);
}

#[test]
fn override_with_typedef_spelling_falls_back_to_arity() {
    let w = world(&["struct A { virtual void f(size_t n); }; struct B : A { void f(unsigned long n) override; };"]);
    assert_eq!(w.family("A::f"), ["A::f", "B::f"]);
    // Without `override` a differing spelling is a different overload.
    let w = world(&["struct A { virtual void f(size_t n); }; struct B : A { virtual void f(unsigned long n); };"]);
    assert_eq!(w.family("A::f"), ["A::f"]);
}

#[test]
fn large_wide_and_deep_hierarchies_scale() {
    let mut src = String::from("struct Root { virtual void update(); virtual ~Root(); };\n");
    for i in 0..3000 {
        let base = if i < 10 { "Root".to_string() } else { format!("C{}", i / 10) };
        src.push_str(&format!("struct C{i} : {base} {{ void update() override; ~C{i}(); }};\n"));
    }
    let t = std::time::Instant::now();
    let w = world(&[&src]);
    assert!(t.elapsed().as_secs() < 5);
    assert_eq!(w.family("Root::update").len(), 3001);
    assert_eq!(w.family("Root::~Root").len(), 3001);
    assert_eq!(w.h.stats.bases_unresolved, 0);
}
