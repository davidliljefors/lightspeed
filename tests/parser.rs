//! Declaration parser tests: preprocess, then parse, then inspect the flat DB.

use lightspeed::workspace::compile_commands::FlagSet;
use lightspeed::base::ids::Idx;
use lightspeed::parser::{OpaqueKind, ParseOutput, parse};
use lightspeed::pp::vfs::MemFs;
use lightspeed::pp::{PpConfig, PpOutput, Preprocessor};
use lightspeed::symbols::{Access, SymbolDb, SymbolFlags, SymbolId, SymbolKind};

struct Parsed {
    db: SymbolDb,
    res: ParseOutput,
    out: PpOutput,
    src: String,
}

fn parse_src(src: &str) -> Parsed {
    let fs = MemFs::new();
    let mut pp = Preprocessor::new(&fs, PpConfig::default());
    pp.set_flags(&FlagSet::default());
    let mut out = PpOutput::default();
    pp.run("/t/main.cpp", src, &mut out);
    let mut db = SymbolDb::new();
    let file = db.files.intern("/t/main.cpp");
    let mut res = ParseOutput::default();
    parse(&out, src, &mut db, file, &mut res);
    Parsed { db, res, out, src: src.to_string() }
}

impl Parsed {
    /// `"Kind qualified::name"` per symbol, in declaration order.
    fn dump(&self) -> Vec<String> {
        self.db
            .symbols()
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{:?} {}", s.kind, self.db.qualified_name(SymbolId::from_usize(i))))
            .collect()
    }

    fn find(&self, qname: &str) -> SymbolId {
        (0..self.db.len())
            .map(SymbolId::from_usize)
            .find(|&id| self.db.qualified_name(id) == qname)
            .unwrap_or_else(|| panic!("no symbol {qname}; have {:?}", self.dump()))
    }

    fn has(&self, qname: &str, flag: SymbolFlags) -> bool {
        self.db.symbol(self.find(qname)).flags.contains(flag)
    }

    fn kind(&self, qname: &str) -> SymbolKind {
        self.db.symbol(self.find(qname)).kind
    }

    fn opaque_text(&self, kind: OpaqueKind) -> Vec<String> {
        self.res
            .opaque
            .iter()
            .filter(|o| o.kind == kind)
            .map(|o| {
                let toks = &self.out.tokens[o.start as usize..o.end as usize];
                toks.iter().map(|t| self.out.text(t, &self.src)).collect::<Vec<_>>().join(" ")
            })
            .collect()
    }
}

fn d(src: &str) -> Vec<String> {
    parse_src(src).dump()
}

#[test]
fn namespaces_nested_and_compact() {
    assert_eq!(
        d("namespace a { namespace b { int x; } } namespace c::d { void f(); }"),
        ["Namespace a", "Namespace a::b", "Variable a::b::x", "Namespace c", "Namespace c::d", "Function c::d::f"]
    );
}

#[test]
fn anonymous_namespace_and_extern_c() {
    let p = parse_src("namespace { int hidden; } extern \"C\" { void cfn(int); } extern \"C\" void one();");
    assert_eq!(p.dump(), ["Variable hidden", "Function cfn", "Function one"]);
}

#[test]
fn class_with_bases_and_access() {
    let p = parse_src("class A {}; class B : public A, protected virtual C<int>, D {};");
    let bases = p.db.bases();
    assert_eq!(bases.len(), 3);
    assert_eq!(p.db.name(SymbolId::from_usize(0)), "A");
    assert_eq!(bases[0].access, Access::Public);
    assert!(!bases[0].is_virtual);
    assert_eq!(bases[1].access, Access::Protected);
    assert!(bases[1].is_virtual);
    assert_eq!(bases[2].access, Access::Private, "class default");
}

#[test]
fn struct_defaults_public_class_private() {
    let p = parse_src("struct S { int a; private: int b; }; class C { int a; public: int b; };");
    let acc = |n| p.db.symbol(p.find(n)).access;
    assert_eq!(acc("S::a"), Access::Public);
    assert_eq!(acc("S::b"), Access::Private);
    assert_eq!(acc("C::a"), Access::Private);
    assert_eq!(acc("C::b"), Access::Public);
}

#[test]
fn methods_ctors_dtors_virtuals() {
    let p = parse_src(
        "class Base { public: Base(); explicit Base(int x) : m(x) {} virtual ~Base() = default;
            virtual void tick(float dt) = 0; virtual int get() const; static int count(); int m; };
         class Derived final : public Base { void tick(float dt) override; int get() const final { return 1; } };",
    );
    assert_eq!(p.kind("Base::Base"), SymbolKind::Constructor);
    assert_eq!(p.kind("Base::~Base"), SymbolKind::Destructor);
    assert_eq!(p.kind("Base::tick"), SymbolKind::Method);
    assert!(p.has("Base::tick", SymbolFlags::PURE | SymbolFlags::VIRTUAL));
    assert!(p.has("Base::~Base", SymbolFlags::DEFAULTED | SymbolFlags::VIRTUAL));
    assert!(p.has("Base::get", SymbolFlags::CONST | SymbolFlags::VIRTUAL));
    assert!(p.has("Base::count", SymbolFlags::STATIC));
    assert_eq!(p.kind("Base::m"), SymbolKind::Field);
    assert!(p.has("Derived::tick", SymbolFlags::OVERRIDE));
    assert!(p.has("Derived::get", SymbolFlags::FINAL | SymbolFlags::DEFINITION));
    assert!(p.has("Derived", SymbolFlags::DEFINITION));
    // The ctor with an init list has a body that stays opaque.
    assert_eq!(p.opaque_text(OpaqueKind::Body).len(), 2);
}

#[test]
fn function_bodies_are_opaque_and_not_scanned() {
    let p = parse_src("void f() { class Local {}; int x = 1; if (x) { y(); } } int after;");
    assert_eq!(p.dump(), ["Function f", "Variable after"]);
    assert_eq!(p.opaque_text(OpaqueKind::Body), ["{ class Local { } ; int x = 1 ; if ( x ) { y ( ) ; } }"]);
}

#[test]
fn out_of_line_definitions() {
    let p = parse_src("void Foo::bar(int) {} Foo::Foo() : x(1) {} Foo::~Foo() {} int ns::Foo::val = 3; template<class T> void Foo<T>::baz() {}");
    assert_eq!(p.dump(), ["Method bar", "Constructor Foo", "Destructor ~Foo", "Variable val", "Method baz"]);
    let q: Vec<&str> = p.db.out_of_line().iter().map(|&(_, q)| p.db.names.get(q)).collect();
    assert_eq!(q, ["Foo", "Foo", "Foo", "ns::Foo", "Foo"]);
    assert!(p.has("baz", SymbolFlags::TEMPLATE));
}

#[test]
fn enums_plain_scoped_typed_and_umeta() {
    let p = parse_src(
        "enum Color { Red, Green = 5, Blue = Green + 1, }; enum class E : unsigned char { A, B }; enum Fwd : int; \
         UENUM() enum class EKind : uint8 { One UMETA(DisplayName = \"1\"), Two };",
    );
    assert_eq!(
        p.dump(),
        ["Enum Color", "EnumConstant Color::Red", "EnumConstant Color::Green", "EnumConstant Color::Blue", "Enum E", "EnumConstant E::A", "EnumConstant E::B", "Enum Fwd", "Enum EKind", "EnumConstant EKind::One", "EnumConstant EKind::Two"]
    );
    assert!(!p.has("Fwd", SymbolFlags::DEFINITION));
    assert_eq!(p.opaque_text(OpaqueKind::Annotation).len(), 2, "UENUM() and UMETA(..)");
}

#[test]
fn aliases_and_typedefs() {
    assert_eq!(
        d("using A = int; typedef unsigned long ulong; typedef void (*Fn)(int); typedef struct { int x; } Pt; using namespace std; using std::swap;"),
        ["TypeAlias A", "TypeAlias ulong", "TypeAlias Fn", "Field x", "TypeAlias Pt"]
    );
}

#[test]
fn templates_flag_and_skip_params() {
    let p = parse_src("template<typename T, int N = (1>2)> class Vec { T data[N]; void push(const T&); }; template<class T> T maxv(T a, T b); template class Vec<int>;");
    assert!(p.has("Vec", SymbolFlags::TEMPLATE));
    assert!(p.has("maxv", SymbolFlags::TEMPLATE));
    assert!(!p.has("Vec::push", SymbolFlags::TEMPLATE));
    assert_eq!(p.kind("Vec::data"), SymbolKind::Field);
}

#[test]
fn operators_and_conversions() {
    let p = parse_src("struct V { V& operator=(const V&); bool operator==(const V&) const; int operator()(int) ; explicit operator bool() const; V operator+(V) const; void* operator new(unsigned long); };");
    let names: Vec<String> = p.dump();
    assert!(names.contains(&"Method V::operator=".to_string()), "{names:?}");
    assert!(names.contains(&"Method V::operator==".to_string()));
    assert!(names.contains(&"Method V::operator()".to_string()));
    assert!(names.contains(&"Method V::operator bool".to_string()));
    assert!(names.contains(&"Method V::operator+".to_string()));
    assert!(names.contains(&"Method V::operator new".to_string()));
}

#[test]
fn fields_multi_declarators_initializers_function_pointers() {
    let p = parse_src("struct S { int a, b = 2, *c; float arr[4] = {1,2,3,4}; void (*cb)(int); int bits : 3; std::vector<int> v{1,2}; eastl::map<int, eastl::string> m; };");
    for f in ["a", "b", "c", "arr", "cb", "bits", "v", "m"] {
        assert_eq!(p.kind(&format!("S::{f}")), SymbolKind::Field, "{f}: {:?}", p.dump());
    }
    assert_eq!(p.db.len(), 9);
}

#[test]
fn static_members_and_globals() {
    let p = parse_src("struct S { static int s; static const int k = 5; }; int g = 1; extern int e; const char* name = \"x\"; static int fs();");
    assert_eq!(p.kind("S::s"), SymbolKind::Variable);
    assert_eq!(p.kind("S::k"), SymbolKind::Variable);
    assert_eq!(p.kind("g"), SymbolKind::Variable);
    assert_eq!(p.kind("e"), SymbolKind::Variable);
    assert_eq!(p.kind("name"), SymbolKind::Variable);
    assert_eq!(p.kind("fs"), SymbolKind::Function);
}

#[test]
fn nested_types_and_friend() {
    let p = parse_src("class Outer { public: struct Inner { enum Kind { X }; int v; }; friend class Other; friend void helper(Outer&); class Fwd; };");
    assert_eq!(p.kind("Outer::Inner"), SymbolKind::Struct);
    assert_eq!(p.kind("Outer::Inner::Kind"), SymbolKind::Enum);
    assert_eq!(p.kind("Outer::Inner::Kind::X"), SymbolKind::EnumConstant);
    assert_eq!(p.kind("Outer::Fwd"), SymbolKind::Class);
    assert!(!p.has("Outer::Fwd", SymbolFlags::DEFINITION));
    assert!(p.dump().iter().all(|s| !s.contains("helper") && !s.contains("Other")));
}

#[test]
fn forward_declarations_and_elaborated_types() {
    let p = parse_src("class Fwd; struct S* make(); class Real *global_ptr; struct Tag { } tag_var;");
    assert_eq!(p.kind("Fwd"), SymbolKind::Class);
    assert_eq!(p.kind("make"), SymbolKind::Function);
    assert_eq!(p.kind("global_ptr"), SymbolKind::Variable);
    assert_eq!(p.kind("Tag"), SymbolKind::Struct);
}

#[test]
fn unreal_style_annotations() {
    let src = "UCLASS(Blueprintable, meta=(Foo=\"a)b\"))
class ENGINE_API AMyActor : public AActor {
    GENERATED_BODY()
public:
    UPROPERTY(EditAnywhere, BlueprintReadWrite)
    float Speed;
    UFUNCTION(BlueprintCallable)
    virtual void Fire(int Count) override;
    DECLARE_DELEGATE(FOnFire);
    ENGINE_API void Exported();
    UE_DEPRECATED(5.0, \"x\") void Old();
};
DECLARE_LOG_CATEGORY_EXTERN(LogFoo, Log, All);";
    let p = parse_src(src);
    assert_eq!(p.kind("AMyActor"), SymbolKind::Class);
    assert_eq!(p.db.bases().len(), 1);
    assert_eq!(p.db.name(SymbolId::from_usize(0)), "AMyActor");
    assert_eq!(p.kind("AMyActor::Speed"), SymbolKind::Field);
    assert!(p.has("AMyActor::Fire", SymbolFlags::VIRTUAL | SymbolFlags::OVERRIDE));
    assert_eq!(p.kind("AMyActor::Exported"), SymbolKind::Method);
    assert_eq!(p.kind("AMyActor::Old"), SymbolKind::Method);
    let ann = p.opaque_text(OpaqueKind::Annotation);
    assert!(ann.iter().any(|a| a.starts_with("UCLASS (")));
    assert!(ann.iter().any(|a| a.starts_with("GENERATED_BODY")));
    assert!(ann.iter().any(|a| a.starts_with("UPROPERTY")));
    assert!(ann.iter().any(|a| a.starts_with("UFUNCTION")));
    let mac = p.opaque_text(OpaqueKind::Macro);
    assert_eq!(mac.len(), 2, "{mac:?}");
    // UCLASS attaches to the class, UPROPERTY to the field.
    let owner_of = |prefix: &str| {
        let o = p.res.opaque.iter().find(|o| p.opaque_text_at(o).starts_with(prefix)).unwrap();
        p.db.qualified_name(o.owner.expect("annotation has an owner"))
    };
    assert_eq!(owner_of("UCLASS"), "AMyActor");
    assert_eq!(owner_of("UPROPERTY"), "AMyActor::Speed");
    assert_eq!(owner_of("UFUNCTION"), "AMyActor::Fire");
}

impl Parsed {
    fn opaque_text_at(&self, o: &lightspeed::parser::OpaqueTokenBlock) -> String {
        self.out.tokens[o.start as usize..o.end as usize].iter().map(|t| self.out.text(t, &self.src)).collect::<Vec<_>>().join(" ")
    }
}

#[test]
fn extents_and_name_offsets_point_into_source() {
    let src = "namespace n {\n  class Foo : Bar {\n    void m() { }\n  };\n}\n";
    let p = parse_src(src);
    for id in (0..p.db.len()).map(SymbolId::from_usize) {
        let s = p.db.symbol(id);
        let (a, b) = p.db.name_range(id);
        assert_eq!(&src[a as usize..b as usize], p.db.name(id).trim_start_matches('~'), "name range of {}", p.db.qualified_name(id));
        assert!(s.extent_start <= s.name_offset && s.name_offset < s.extent_end.max(s.name_offset + 1));
        assert!(s.extent_end as usize <= src.len());
    }
    let foo = p.db.symbol(p.find("n::Foo"));
    assert!(src[foo.extent_start as usize..foo.extent_end as usize].ends_with('}'));
    let ns = p.db.symbol(p.find("n"));
    assert!(src[ns.extent_start as usize..ns.extent_end as usize].ends_with('}'));
}

#[test]
fn parents_precede_children() {
    let p = parse_src("namespace a { class B { struct C { void f(); }; enum E { X }; }; }");
    for (i, s) in p.db.symbols().iter().enumerate() {
        if let Some(par) = s.parent {
            assert!(par.index() < i);
        }
    }
}

#[test]
fn inactive_code_is_not_parsed() {
    let p = parse_src("#define ON 0\n#if ON\nclass Hidden {};\n#else\nclass Shown {};\n#endif\n");
    assert_eq!(p.dump(), ["Class Shown", "Macro ON"]);
}

#[test]
fn macro_generated_declarations() {
    let p = parse_src("#define DECL(n) class n { int v; };\nDECL(Gen)\n#define API\nclass API Exp {};\n");
    assert_eq!(p.dump(), ["Class Gen", "Field Gen::v", "Class Exp", "Macro DECL", "Macro API"]);
}

#[test]
fn garbage_never_panics_and_progresses() {
    let garbage = [
        "", "}", "}}}{{{", "class", "class {", "class A : {", "struct", "enum", "enum {", "enum class", "namespace", "namespace {", "namespace a::",
        "template<", "template<>", "operator", "int operator", "void f(", "void f(int", "int x = ", "using", "using =", "typedef", "typedef ;", "friend", "extern \"C\"",
        "a::b::c::", "~", "~;", "A::~", "( ) ( ) ( )", "[ [ [", "class A { class B { class C {", "int f() const noexcept(", "= = = =", "virtual = 0;", ":: ;",
        "UCLASS(", "UCLASS()", "FOO() BAR() BAZ()", ";;;;;", "enum E : { A }", "void (*)(int);", "int (*)", "decltype(", "static_assert(",
    ];
    for g in garbage {
        let _ = parse_src(g);
        let _ = parse_src(&format!("namespace n {{ {g} }} class After {{}};"));
    }
}

#[test]
fn deep_nesting_is_bounded() {
    let n = 5000;
    let src = format!("{}int x;{}", "namespace a {".repeat(n), "}".repeat(n));
    let p = parse_src(&src);
    assert!(p.res.opaque.iter().any(|o| o.kind == OpaqueKind::Skipped));
    let src = format!("{}{}", "class A { ".repeat(n), "};".repeat(n));
    let _ = parse_src(&src);
}

#[test]
fn fragment_soup_fuzz() {
    const FR: &[&str] = &[
        "class", "struct", "enum", "namespace", "template", "<", ">", "{", "}", "(", ")", "[", "]", ";", ",", ":", "::", "=", "0", "virtual", "override", "const", "static", "operator", "~",
        "X", "Y", "Z", "int", "void", "UCLASS()", "GENERATED_BODY()", "public:", "using", "typedef", "friend", "final", "...", "->", "&", "*", "\"s\"", "1",
    ];
    let mut seed = 0x9E3779B97F4A7C15u64;
    for _ in 0..3000 {
        let mut s = String::new();
        for _ in 0..(seed % 40 + 1) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            s.push_str(FR[(seed % FR.len() as u64) as usize]);
            s.push(' ');
        }
        let p = parse_src(&s);
        for (i, sym) in p.db.symbols().iter().enumerate() {
            if let Some(par) = sym.parent {
                assert!(par.index() < i, "parent order violated for {s:?}");
            }
            assert!(sym.extent_end >= sym.extent_start);
        }
        for o in &p.res.opaque {
            assert!(o.start <= o.end && o.end as usize <= p.out.tokens.len(), "{s:?}");
        }
    }
}

#[test]
fn eastl_style_code() {
    let p = parse_src(
        "namespace eastl { template <typename T, typename Allocator = allocator> class vector : public vector_base<T, Allocator> {
            public: typedef vector<T, Allocator> this_type; typedef T value_type; using iterator = T*;
            vector() EA_NOEXCEPT_IF(true); explicit vector(size_type n, const allocator_type& a = EASTL_VECTOR_DEFAULT_ALLOCATOR);
            this_type& operator=(this_type&& x); void push_back(const value_type& v); iterator begin() EA_NOEXCEPT; reference operator[](size_type n);
            protected: value_type* mpBegin; };
          template <typename T> inline void swap(vector<T>& a, vector<T>& b) EA_NOEXCEPT { a.swap(b); } }",
    );
    for n in ["eastl::vector", "eastl::vector::push_back", "eastl::vector::begin", "eastl::vector::operator[]", "eastl::vector::operator=", "eastl::vector::mpBegin", "eastl::swap", "eastl::vector::iterator", "eastl::vector::vector"] {
        p.find(n);
    }
    assert_eq!(p.kind("eastl::vector::vector"), SymbolKind::Constructor);
    assert_eq!(p.db.symbol(p.find("eastl::vector::mpBegin")).access, Access::Protected);
}
