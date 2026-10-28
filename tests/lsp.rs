//! Protocol-level tests: drive the server with JSON-RPC messages.

mod common;

use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};

use lightspeed::lsp::{Server, offset_to_position, path_to_uri, position_to_offset, read_frame, uri_to_path, write_frame};
use lightspeed::base::source::LineIndex;
use serde_json::{Value, json};

struct Client {
    server: Server,
    next: i64,
}

impl Client {
    fn new(root: &str) -> Self {
        let mut c = Self { server: Server::new(None), next: 1 };
        let r = c.request("initialize", json!({"rootUri": path_to_uri(root), "capabilities": {}}));
        assert!(r["result"]["capabilities"]["definitionProvider"].as_bool().unwrap());
        c.notify("initialized", json!({}));
        c
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let mut out = Vec::new();
        self.server.handle(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}), &mut out);
        out.into_iter().find(|m| m["id"] == id).expect("response")
    }

    fn notify(&mut self, method: &str, params: Value) {
        let mut out = Vec::new();
        self.server.handle(json!({"jsonrpc": "2.0", "method": method, "params": params}), &mut out);
    }

    fn open(&mut self, path: &str, text: &str) {
        self.notify("textDocument/didOpen", json!({"textDocument": {"uri": path_to_uri(path), "languageId": "cpp", "version": 1, "text": text}}));
    }

    fn change(&mut self, path: &str, text: &str) {
        self.notify("textDocument/didChange", json!({"textDocument": {"uri": path_to_uri(path)}, "contentChanges": [{"text": text}]}));
    }
}

fn pos_of(text: &str, needle: &str, nth: usize) -> Value {
    let off = text.match_indices(needle).nth(nth).unwrap().0 as u32;
    let (line, character) = offset_to_position(text, &LineIndex::new(text), off);
    json!({"line": line, "character": character})
}

fn tmp() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().to_str().unwrap().replace('\\', "/");
    (d, r)
}

#[test]
fn uri_roundtrip_and_decoding() {
    let decoded = uri_to_path("file:///c%3A/dev/My%20Proj/a.cpp").unwrap();
    assert_eq!(decoded, if cfg!(windows) { "c:/dev/My Proj/a.cpp" } else { "/c:/dev/My Proj/a.cpp" });
    for p in ["C:/dev/lightspeed/src/a b.cpp", "C:/dev/ünï/x.h", "/usr/include/a#b.h"] {
        let back = uri_to_path(&path_to_uri(p)).unwrap();
        assert_eq!(back, p, "{p}");
    }
    assert!(uri_to_path("http://x").is_none());
}

#[test]
fn utf16_columns() {
    let t = "a😀b\nñx";
    let li = LineIndex::new(t);
    let b = t.find('b').unwrap() as u32;
    assert_eq!(offset_to_position(t, &li, b), (0, 3));
    assert_eq!(position_to_offset(t, &li, 0, 3), b);
    assert_eq!(position_to_offset(t, &li, 1, 1), t.find('x').unwrap() as u32);
    assert_eq!(position_to_offset(t, &li, 0, 99), t.find('\n').unwrap() as u32);
    assert_eq!(position_to_offset(t, &li, 9, 0), t.len() as u32);
}

#[test]
fn frames_roundtrip() {
    let mut buf = Vec::new();
    write_frame(&mut buf, &json!({"a": 1})).unwrap();
    write_frame(&mut buf, &json!({"b": "é"})).unwrap();
    let mut r = BufReader::new(&buf[..]);
    assert_eq!(serde_json::from_slice::<Value>(&read_frame(&mut r).unwrap().unwrap()).unwrap(), json!({"a": 1}));
    assert_eq!(serde_json::from_slice::<Value>(&read_frame(&mut r).unwrap().unwrap()).unwrap(), json!({"b": "é"}));
    assert!(read_frame(&mut r).unwrap().is_none());
}

#[test]
fn definition_references_and_live_edits() {
    let (_d, root) = tmp();
    let h = format!("{root}/shape.h");
    let a = format!("{root}/a.cpp");
    let htxt = "#pragma once\nstruct Shape { virtual float area(); };\n";
    let atxt = "#include \"shape.h\"\nfloat Shape::area() { return 1; }\nfloat use(Shape* s) { return s->area(); }\n";
    std::fs::write(&h, htxt).unwrap();
    std::fs::write(&a, atxt).unwrap();
    common::write_cdb(&root, true);

    let mut c = Client::new(&root);
    c.open(&a, atxt);

    // s->area() jumps to the out-of-line implementation.
    let r = c.request("textDocument/definition", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": pos_of(atxt, "area", 1)}));
    let locs = r["result"].as_array().unwrap();
    assert_eq!(locs.len(), 1);
    assert_eq!(locs[0]["uri"], path_to_uri(&a));
    assert_eq!(locs[0]["range"]["start"]["line"], 1);

    let r = c.request("textDocument/references", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": pos_of(atxt, "area", 1), "context": {"includeDeclaration": true}}));
    assert_eq!(r["result"].as_array().unwrap().len(), 3);

    // An unsaved edit is visible to the very next request.
    let edited = atxt.replace("float use", "int newfn() { return 0; }\nfloat use");
    c.change(&a, &edited);
    let r = c.request("workspace/symbol", json!({"query": "newfn"}));
    assert_eq!(r["result"].as_array().unwrap().len(), 1);
    let r = c.request("textDocument/documentSymbol", json!({"textDocument": {"uri": path_to_uri(&a)}}));
    let names: Vec<&str> = r["result"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"newfn") && names.contains(&"use"), "{names:?}");

    // Closing reverts to disk contents.
    c.notify("textDocument/didClose", json!({"textDocument": {"uri": path_to_uri(&a)}}));
    let r = c.request("workspace/symbol", json!({"query": "newfn"}));
    assert!(r["result"].as_array().unwrap().is_empty());
}

/// Ctrl-click on a function that is defined right where the cursor is has nowhere to go.
/// Answering with its own location makes editors start a find-references search instead.
#[test]
fn definition_on_the_definition_itself_returns_nothing() {
    let (_d, root) = tmp();
    let h = format!("{root}/example.h");
    let htxt = "#pragma once\nclass Example {\npublic:\n  static Example* create() { return new Example(); }\n  int count(int n) { int local = n; return local; }\n  void declared_only();\n};\nExample* make() { return Example::create(); }\n";
    std::fs::write(&h, htxt).unwrap();
    write_cdb_for(&root, &["a.cpp"]);
    std::fs::write(format!("{root}/a.cpp"), "#include \"example.h\"\n").unwrap();

    let mut c = Client::new(&root);
    c.open(&h, htxt);
    let mut def = |needle: &str, nth: usize| c.request("textDocument/definition", json!({"textDocument": {"uri": path_to_uri(&h)}, "position": pos_of(htxt, needle, nth)}))["result"].clone();
    assert_eq!(def("create()", 0), json!([]), "inline definition");
    assert_eq!(def("local =", 0), json!([]), "local declaration");
    assert_eq!(def("declared_only", 0), json!([]), "declaration without an implementation");
    // From a use, it still jumps.
    assert_eq!(def("create()", 1).as_array().unwrap().len(), 1);
    assert_eq!(def("local;", 0).as_array().unwrap().len(), 1);
}

/// What "find references" of `Example::create` must and must not return in ordinary code.
/// Every body line ending in `//=` is an expected hit; any other line is an expected miss.
/// Bodies are checked once with `std` and once with `eastl` standing in for `NS`.
mod ref_precision {
    use super::*;
    use std::collections::BTreeSet;

    const STUBS: &str = r#"
namespace NS {
template<class T> struct vector {
  T* begin(); T* end(); T& back(); T& front(); T& operator[](int i); T& at(int i);
  void push_back(const T& v); T& emplace_back(); int size();
};
template<class A, class B> struct pair { A first; B second; };
template<class K, class V> struct map {
  struct iterator { pair<const K, V>& operator*(); pair<const K, V>* operator->(); iterator& operator++(); bool operator!=(const iterator& o); };
  iterator begin(); iterator end(); iterator find(const K& k); V& operator[](const K& k); V& at(const K& k);
};
template<class T> struct unique_ptr { T* operator->(); T& operator*(); T* get(); };
template<class T> struct shared_ptr { T* operator->(); T& operator*(); T* get(); };
template<class T> struct optional { T* operator->(); T& operator*(); T& value(); };
template<class T, class... A> unique_ptr<T> make_unique(A... a);
template<class T, class... A> shared_ptr<T> make_shared(A... a);
}
struct Other { static Other* create(); Other* self(); int create_count; };
"#;

    fn check(body: &str, cursor: &str) {
        let mut errors = Vec::new();
        for ns in ["std", "eastl"] {
            let src = format!("{}{}", STUBS.replace("NS", ns), body.replace("NS", ns));
            let (_d, root) = tmp();
            let a = format!("{root}/a.cpp");
            std::fs::write(&a, &src).unwrap();
            write_cdb_for(&root, &["a.cpp"]);
            let mut c = Client::new(&root);
            c.open(&a, &src);
            let at = src.find(cursor).unwrap() + cursor.find("create").unwrap();
            let (line, character) = offset_to_position(&src, &LineIndex::new(&src), at as u32);
            let r = c.request("textDocument/references", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": {"line": line, "character": character}, "context": {"includeDeclaration": true}}));
            let got: BTreeSet<usize> = r["result"].as_array().unwrap().iter().map(|l| l["range"]["start"]["line"].as_u64().unwrap() as usize).collect();
            let lines: Vec<&str> = src.lines().collect();
            let want: BTreeSet<usize> = (0..lines.len()).filter(|&i| lines[i].trim_end().ends_with("//=")).collect();
            for &i in want.difference(&got) {
                errors.push(format!("[{ns}] missing: {}", lines[i].trim()));
            }
            for &i in got.difference(&want) {
                errors.push(format!("[{ns}] extra:   {}", lines[i].trim()));
            }
        }
        assert!(errors.is_empty(), "\n{}", errors.join("\n"));
    }

    #[test]
    fn receivers_through_containers() {
        check(
            r#"
struct Example { static Example* create();  //=
  Example* self(); };
Example& get();
Other& other();
void t(NS::vector<Example> xs, NS::vector<Example*> ps, NS::vector<Other> os, NS::map<int, Example> m, NS::pair<int, Example> pr, Example* p, Example& r, const Example& cr) {
  Example::create();  //=
  Other::create();
  xs[0].create();  //=
  xs.back().create();  //=
  xs.at(1).create();  //=
  xs.emplace_back().create();  //=
  auto x = xs.back(); x.create();  //=
  auto& y = xs.front(); y.create();  //=
  auto it = xs.begin(); it->create();  //=
  ps[0]->create();  //=
  for (auto e : ps) e->create();  //=
  for (auto& e : xs) e.create();  //=
  for (const auto& e : xs) e.create();  //=
  for (auto& o : os) o.create();
  os[0].create();
  m[1].create();  //=
  m.at(1).create();  //=
  m.find(1)->second.create();  //=
  auto f = m.find(1); f->second.create();  //=
  pr.second.create();  //=
  p->create();  //=
  r.create();  //=
  cr.create();  //=
  get().create();  //=
  other().create();
  p->self()->create();  //=
  auto z = os.back(); z.create();
  auto q = Example::create(); q->create();  //=
}
"#,
            "Example* create(",
        );
    }

    #[test]
    fn structured_bindings_and_lambdas() {
        check(
            r#"
struct Example { static Example* create();  //=
};
void t(NS::map<int, Example> m, NS::map<int, Other> mo, NS::vector<Example> xs, NS::pair<int, Example> pr) {
  for (auto& [k, v] : m) v.create();  //=
  for (const auto& [k, v] : m) v.create();  //=
  for (auto& [k, v] : mo) v.create();
  auto [a, b] = pr; b.create();  //=
  auto& [c, d] = pr; d.create();  //=
  auto cb = [](Example& e) { e.create(); };  //=
  auto cb2 = [&](int i) { xs[i].create(); };  //=
  auto cb3 = [&]() { for (auto& [k, v] : m) v.create(); };  //=
  if (auto it = m.find(2); it != m.end()) it->second.create();  //=
  if (auto* e = &xs[0]) e->create();  //=
}
"#,
            "Example* create(",
        );
    }

    #[test]
    fn smart_pointers_and_factories() {
        check(
            r#"
struct Example { static Example* create();  //=
  static NS::unique_ptr<Example> make(); };
void t(NS::unique_ptr<Example> up, NS::shared_ptr<Example> sp, NS::optional<Example> op, NS::unique_ptr<Other> uo) {
  up->create();  //=
  sp->create();  //=
  op->create();  //=
  op.value().create();  //=
  (*up).create();  //=
  up.get()->create();  //=
  uo->create();
  auto mu = NS::make_unique<Example>(); mu->create();  //=
  auto ms = NS::make_shared<Example>(); ms->create();  //=
  auto mo = NS::make_unique<Other>(); mo->create();
  Example::make()->create();  //=
  auto f = Example::make(); f->create();  //=
}
"#,
            "Example* create(",
        );
    }

    #[test]
    fn overloads_by_arguments() {
        check(
            r#"
struct Example { static Example* create(int n);  //=
  static Example* create(const char* s);
  static Example* create(); };
void t(int n, const char* s, long l) {
  Example::create(1);  //=
  Example::create(n);  //=
  Example::create(n + 1);  //=
  Example::create(l);  //=
  Example::create("x");
  Example::create(s);
  Example::create();
}
"#,
            "create(int",
        );
    }

    #[test]
    fn scopes_inheritance_and_aliases() {
        check(
            r#"
namespace a { struct Example { static Example* create();  //=
  void run() { create(); }  //=
  void run2() { Example::create(); }  //=
  void run3() { this->create(); }  //=
}; }
namespace b { struct Example { static Example* create(); void run() { create(); } }; void f(Example e) { e.create(); } }
namespace a { void f(Example e) { e.create(); }  //=
}
struct Derived : a::Example { void go() { create(); }  //=
};
struct Unrelated { void create(); void go() { create(); } };
using Alias = a::Example;
typedef a::Example Typedef;
namespace c { using a::Example; void f() { Example::create(); } }  //=
void t(Derived d, Alias al, Typedef td, b::Example be, Unrelated u) {
  a::Example::create();  //=
  b::Example::create();
  Derived::create();  //=
  d.create();  //=
  Alias::create();  //=
  al.create();  //=
  Typedef::create();  //=
  td.create();  //=
  be.create();
  u.create();
  a::Example e; e.create();  //=
  b::Example f; f.create();
  Other::create();
}
"#,
            "Example* create(",
        );
    }
}

/// Like `common::write_cdb`, but lists every given translation unit.
fn write_cdb_for(root: &str, files: &[&str]) {
    let cdb: Vec<Value> = files.iter().map(|f| json!({"directory": root, "file": f, "arguments": ["clang++", "-c", f, "-I", root]})).collect();
    std::fs::write(format!("{root}/compile_commands.json"), serde_json::to_string(&cdb).unwrap()).unwrap();
}

#[test]
fn includes_are_document_links() {
    let (_d, root) = tmp();
    let (q, s) = (format!("{root}/q.h"), format!("{root}/sys.h"));
    std::fs::write(&q, "#pragma once\n").unwrap();
    std::fs::write(&s, "#pragma once\n").unwrap();
    let a = format!("{root}/a.cpp");
    let txt = "#include \"q.h\"\n  #  include <sys.h>\n#include \"missing.h\"\n#define H \"q.h\"\n#include H\n#if 0\n#include \"q.h\"\n#endif\nint x;\n";
    std::fs::write(&a, txt).unwrap();
    common::write_cdb(&root, true);

    let mut c = Client::new(&root);
    c.open(&a, txt);
    let r = c.request("textDocument/documentLink", json!({"textDocument": {"uri": path_to_uri(&a)}}));
    let links = r["result"].as_array().unwrap();
    assert_eq!(links.len(), 2, "{r}");
    let span = |l: &Value| {
        let (line, s, e) = (l["range"]["start"]["line"].as_u64().unwrap() as usize, l["range"]["start"]["character"].as_u64().unwrap() as usize, l["range"]["end"]["character"].as_u64().unwrap() as usize);
        txt.lines().nth(line).unwrap()[s..e].to_owned()
    };
    assert_eq!((span(&links[0]), links[0]["target"].clone()), ("q.h".to_owned(), json!(path_to_uri(&q))));
    assert_eq!((span(&links[1]), links[1]["target"].clone()), ("sys.h".to_owned(), json!(path_to_uri(&s))));

    // Edits shift the links with the buffer.
    let edited = format!("// new\n{txt}");
    c.change(&a, &edited);
    let r = c.request("textDocument/documentLink", json!({"textDocument": {"uri": path_to_uri(&a)}}));
    assert_eq!(r["result"][0]["range"]["start"]["line"], 1, "{r}");
}

#[test]
fn type_hierarchy_and_inheritance_graph_over_rpc() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let txt = "struct A {};\nstruct B : A {};\nstruct C : B {};\nstruct D : B {};\nstruct Other {};\nB* make();\n";
    std::fs::write(&a, txt).unwrap();
    common::write_cdb(&root, false);
    let mut c = Client::new(&root);
    c.open(&a, txt);
    let at = json!({"textDocument": {"uri": path_to_uri(&a)}, "position": pos_of(txt, "B* make", 0)});

    let r = c.request("textDocument/prepareTypeHierarchy", at.clone());
    let item = r["result"][0].clone();
    assert_eq!(item["name"], "B");
    let names = |v: &Value| -> Vec<String> { let mut n: Vec<String> = v["result"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_owned()).collect(); n.sort(); n };
    assert_eq!(names(&c.request("typeHierarchy/supertypes", json!({"item": item}))), ["A"]);
    assert_eq!(names(&c.request("typeHierarchy/subtypes", json!({"item": item}))), ["C", "D"]);

    let g = c.request("lightspeed/inheritanceGraph", at)["result"].clone();
    assert_eq!(g["nodes"].as_array().unwrap().len(), 4);
    assert_eq!(g["edges"].as_array().unwrap().len(), 3);
    assert_eq!(g["truncated"], false);
    let g = c.request("lightspeed/inheritanceGraph", json!({"typeName": "B", "maxNodes": 2}))["result"].clone();
    assert_eq!(g["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(g["truncated"], true);

    // Cursor on a declaration, in a namespace, with `public` bases and a template base.
    let t2 = "namespace n {\ntemplate<class T> struct Base { virtual void f(); };\nclass Mid : public Base<int> {};\nclass Leaf final : public Mid {};\n}\n";
    let b = format!("{root}/b.cpp");
    std::fs::write(&b, t2).unwrap();
    c.open(&b, t2);
    for (needle, want) in [("Mid :", "Mid"), ("Leaf final", "Leaf"), ("Base {", "Base"), ("Base<int>", "Base")] {
        let r = c.request("textDocument/prepareTypeHierarchy", json!({"textDocument": {"uri": path_to_uri(&b)}, "position": pos_of(t2, needle, 0)}));
        assert_eq!(r["result"][0]["name"], want, "{needle}: {r}");
    }
    let r = c.request("textDocument/prepareTypeHierarchy", json!({"textDocument": {"uri": path_to_uri(&b)}, "position": pos_of(t2, "Mid :", 0)}));
    let item = r["result"][0].clone();
    assert_eq!(names(&c.request("typeHierarchy/supertypes", json!({"item": item}))), ["Base"]);
    assert_eq!(names(&c.request("typeHierarchy/subtypes", json!({"item": item}))), ["Leaf"]);

    // Classes typed into an open buffer are visible to the very next request.
    let t3 = format!("{t2}struct Typed : n::Mid {{}};\nstruct Typed2 : Typed {{}};\n");
    c.change(&b, &t3);
    let pos = |needle: &str| json!({"textDocument": {"uri": path_to_uri(&b)}, "position": pos_of(&t3, needle, 0)});
    let r = c.request("textDocument/prepareTypeHierarchy", pos("Typed :"));
    assert_eq!(r["result"][0]["name"], "Typed", "{r}");
    let item = r["result"][0].clone();
    assert_eq!(names(&c.request("typeHierarchy/supertypes", json!({"item": item}))), ["Mid"]);
    assert_eq!(names(&c.request("typeHierarchy/subtypes", json!({"item": item}))), ["Typed2"]);

    // Typing a new class character by character in a file that includes a header
    // (and in a header no translation unit includes).
    let hh = format!("{root}/h.h");
    let h_txt = "#pragma once\nstruct HBase { virtual void f(); };\n";
    let m = format!("{root}/m.cpp");
    let m_txt = "#include \"h.h\"\nstruct Existing : HBase {};\n";
    std::fs::write(&hh, h_txt).unwrap();
    std::fs::write(&m, m_txt).unwrap();
    c.open(&m, m_txt);
    let orphan = format!("{root}/orphan.h");
    std::fs::write(&orphan, "").unwrap();
    c.open(&orphan, "");
    let mut buf = m_txt.to_owned();
    let mut obuf = String::new();
    let otyped = "struct Orph {};\n";
    let typed = "struct Fresh : public HBase {};\n";
    for (ch, och) in typed.chars().zip(otyped.chars().chain(std::iter::repeat('\n'))) {
        buf.push(ch);
        obuf.push(och);
        c.change(&m, &buf);
        c.change(&orphan, &obuf);
        let r = c.request("textDocument/prepareTypeHierarchy", json!({"textDocument": {"uri": path_to_uri(&m)}, "position": pos_of(&buf, "HBase", 0)}));
        assert!(r["result"].is_array() || r["result"].is_null(), "{r}");
    }
    let r = c.request("textDocument/prepareTypeHierarchy", json!({"textDocument": {"uri": path_to_uri(&m)}, "position": pos_of(&buf, "Fresh", 0)}));
    assert_eq!(r["result"][0]["name"], "Fresh", "{r}");
    let item = r["result"][0].clone();
    assert_eq!(names(&c.request("typeHierarchy/supertypes", json!({"item": item}))), ["HBase"]);
    let hb = c.request("textDocument/prepareTypeHierarchy", json!({"textDocument": {"uri": path_to_uri(&m)}, "position": pos_of(&buf, "HBase", 0)}));
    let subs = names(&c.request("typeHierarchy/subtypes", json!({"item": hb["result"][0]})));
    assert_eq!(subs, ["Existing", "Fresh"]);
    let r = c.request("textDocument/prepareTypeHierarchy", json!({"textDocument": {"uri": path_to_uri(&orphan)}, "position": pos_of(&obuf, "Orph", 0)}));
    assert_eq!(r["result"][0]["name"], "Orph", "orphan header: {r}");

    // An edit between prepare and the follow-up request renumbers symbols; the item still resolves.
    let hb_item = hb["result"][0].clone();
    let shifted = format!("struct Inserted {{}};\nstruct Inserted2 : HBase {{}};\n{buf}");
    c.change(&m, &shifted);
    let subs = names(&c.request("typeHierarchy/subtypes", json!({"item": hb_item})));
    assert!(subs.contains(&"Inserted2".to_owned()) && subs.contains(&"Fresh".to_owned()), "{subs:?}");
}

#[test]
fn declaration_implementation_and_type_definition_over_rpc() {
    let (_d, root) = tmp();
    let h = "#pragma once\nstruct Shape { virtual void draw() = 0; void name(); };\nstruct Circle : Shape { void draw() override; };\nstruct Sq : Shape { void draw() override; };\n";
    let a = "#include \"h.h\"\nvoid Circle::draw() {}\nvoid Shape::name() {}\nShape g;\nvoid use(Shape* s, Circle& c) {\n  s->draw();\n  c.name();\n  auto loc = c;\n  loc.draw();\n  g.name();\n}\n";
    let (hp, ap) = (format!("{root}/h.h"), format!("{root}/a.cpp"));
    std::fs::write(&hp, h).unwrap();
    std::fs::write(&ap, a).unwrap();
    common::write_cdb(&root, false);
    let mut c = Client::new(&root);
    c.open(&ap, a);
    let at = |needle: &str, nth: usize, delta: usize| {
        let mut p = pos_of(a, needle, nth);
        p["character"] = json!(p["character"].as_u64().unwrap() + delta as u64);
        json!({"textDocument": {"uri": path_to_uri(&ap)}, "position": p})
    };
    let places = |r: Value| -> Vec<String> {
        let mut v: Vec<String> = r["result"].as_array().unwrap().iter().map(|l| format!("{}:{}", l["uri"].as_str().unwrap().rsplit('/').next().unwrap(), l["range"]["start"]["line"])).collect();
        v.sort();
        v
    };

    // Overrides of a pure virtual called through the base: Circle's body and Sq's declaration.
    assert_eq!(places(c.request("textDocument/implementation", at("draw();", 0, 0))), ["a.cpp:1", "h.h:3"]);
    // Classes: the derived ones.
    assert_eq!(places(c.request("textDocument/implementation", at("Shape* s", 0, 0))), ["h.h:2", "h.h:3"]);
    // A non-virtual function: its body.
    assert_eq!(places(c.request("textDocument/implementation", at("name();", 0, 0))), ["a.cpp:2"]);

    // Declaration: from a use and from the out-of-line definition, the in-class one.
    assert_eq!(places(c.request("textDocument/declaration", at("name();", 0, 0))), ["h.h:1"]);
    assert_eq!(places(c.request("textDocument/declaration", at("Circle::draw", 0, 8))), ["h.h:2"]);
    // Definition still prefers the body.
    assert_eq!(places(c.request("textDocument/definition", at("name();", 0, 0))), ["a.cpp:2"]);

    // Type definition of a global, a parameter, an `auto` local, and a member call's receiver.
    assert_eq!(places(c.request("textDocument/typeDefinition", at("g;", 0, 0))), ["h.h:1"]);
    assert_eq!(places(c.request("textDocument/typeDefinition", at("c.name", 0, 0))), ["h.h:2"]);
    assert_eq!(places(c.request("textDocument/typeDefinition", at("loc.draw", 0, 0))), ["h.h:2"]);
    assert_eq!(places(c.request("textDocument/typeDefinition", at("Circle& c", 0, 0))), ["h.h:2"]);
}

/// Decodes `semanticTokens` data into `(line, col, len, type name, modifier bits)`.
fn decode_tokens(r: &Value) -> Vec<(u32, u32, u32, String, u32)> {
    let legend = ["namespace", "class", "struct", "enum", "enumMember", "function", "method", "property", "macro", "type", "variable", "parameter", "typeParameter", "string"];
    let d: Vec<u32> = r["result"]["data"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
    let (mut line, mut col) = (0, 0);
    d.chunks(5)
        .map(|c| {
            line += c[0];
            col = if c[0] == 0 { col + c[1] } else { c[1] };
            (line, col, c[2], legend[c[3] as usize].to_owned(), c[4])
        })
        .collect()
}

#[test]
fn semantic_tokens_over_rpc() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let txt = "#define LIMIT 4\nnamespace N { struct S { static int count; virtual void run() = 0; int v; }; enum E { One }; }\nint f(N::S& s) {\n  /* é */ int x = LIMIT + N::One;\n  s.run(); s.v = f(s); return N::S::count;\n}\nint g(int v) { int run = v; return run; }\n";
    std::fs::write(&a, txt).unwrap();
    common::write_cdb(&root, false);
    let mut c = Client::new(&root);
    let caps = c.request("initialize", json!({"rootUri": path_to_uri(&root), "capabilities": {}}));
    assert_eq!(caps["result"]["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"][5], "function");
    c.open(&a, txt);
    let doc = json!({"uri": path_to_uri(&a)});
    let all = decode_tokens(&c.request("textDocument/semanticTokens/full", json!({"textDocument": doc})));
    let find = |line: u32, col: u32| all.iter().find(|t| t.0 == line && t.1 == col).unwrap_or_else(|| panic!("no token at {line}:{col}: {all:?}"));
    assert_eq!(find(1, 10).3, "namespace");
    assert_eq!(find(1, 21).3, "class"); // structs share the class colour
    assert_eq!((find(1, 36).3.as_str(), find(1, 36).4), ("variable", 1 | 8)); // static data member `count`, declared here
    assert_eq!((find(1, 56).3.as_str(), find(1, 56).4), ("method", 4 | 8)); // pure virtual run, declared here
    assert_eq!(find(3, 18).3, "macro"); // after the multi-byte comment, still the right UTF-16 column
    assert_eq!(find(3, 29).3, "enumMember");
    assert_eq!(find(4, 4).3, "method");
    assert_eq!(find(4, 13).3, "property");
    assert_eq!(find(4, 17).3, "function");
    // Locals and parameters win over same-named members.
    assert_eq!(find(2, 12).3, "parameter");
    // Only the declaration carries the `declaration` bit; uses do not.
    assert_eq!((find(2, 12).4, find(4, 2).4), (8, 0));
    assert_eq!((find(3, 14).4, find(6, 19).4, find(6, 35).4), (8, 8, 0));
    assert_eq!((find(4, 4).4, find(4, 17).4), (4, 0), "uses of members: run is pure virtual, f is a use");
    assert_eq!(find(3, 14).3, "variable");
    assert_eq!(find(4, 2).3, "parameter");
    assert_eq!(find(6, 10).3, "parameter");
    assert_eq!(find(6, 19).3, "variable");
    assert_eq!(find(6, 25).3, "parameter");
    assert_eq!(find(6, 35).3, "variable");
    assert!(all.windows(2).all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)), "tokens must be ordered");

    let part = decode_tokens(&c.request("textDocument/semanticTokens/range", json!({"textDocument": doc, "range": {"start": {"line": 4, "character": 0}, "end": {"line": 5, "character": 0}}})));
    assert!(!part.is_empty() && part.iter().all(|t| t.0 == 4), "{part:?}");
}

#[test]
fn semantic_tokens_template_parameters_shadow_same_named_types() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let txt = "struct T {};\ntemplate <typename T, int N> struct Box { T items[N]; };\ntemplate <class T> T first(T a) { T x = a; return x; }\nT global;\n";
    std::fs::write(&a, txt).unwrap();
    common::write_cdb(&root, false);
    let mut c = Client::new(&root);
    c.open(&a, txt);
    let all = decode_tokens(&c.request("textDocument/semanticTokens/full", json!({"textDocument": {"uri": path_to_uri(&a)}})));
    let ty = |line: u32, col: u32| all.iter().find(|t| t.0 == line && t.1 == col).unwrap_or_else(|| panic!("no token at {line}:{col}: {all:?}")).3.clone();
    assert_eq!(ty(0, 7), "class");
    assert_eq!(all.iter().find(|t| (t.0, t.1) == (1, 19)).unwrap().4, 8, "template parameter declaration");
    assert_eq!(all.iter().find(|t| (t.0, t.1) == (1, 42)).unwrap().4, 0, "template parameter use");
    for (l, c) in [(1, 19), (1, 26), (1, 42), (1, 50), (2, 16), (2, 19), (2, 27), (2, 34)] {
        assert_eq!(ty(l, c), "typeParameter", "{l}:{c}");
    }
    assert_eq!(ty(2, 36), "variable");
    assert_eq!(ty(3, 0), "class", "outside the template the global type is meant");
}

#[test]
fn semantic_tokens_delta_applies_to_previous_result() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let txt = "struct S { int v; };\nint f(S& s) { return s.v; }\nint g(S& t) { return t.v; }\n";
    std::fs::write(&a, txt).unwrap();
    common::write_cdb(&root, false);
    let mut c = Client::new(&root);
    c.open(&a, txt);
    let doc = json!({"uri": path_to_uri(&a)});
    let ints = |v: &Value| -> Vec<u64> { v.as_array().unwrap().iter().map(|x| x.as_u64().unwrap()).collect() };
    let full = c.request("textDocument/semanticTokens/full", json!({"textDocument": doc}))["result"].clone();
    let mut data = ints(&full["data"]);
    let mut prev = full["resultId"].as_str().unwrap().to_owned();

    // Unchanged: no edits.
    let r = c.request("textDocument/semanticTokens/full/delta", json!({"textDocument": doc, "previousResultId": prev}))["result"].clone();
    assert_eq!(r["edits"].as_array().map(Vec::len), Some(0), "{r}");
    prev = r["resultId"].as_str().unwrap().to_owned();

    // Rename a parameter and its use, and add a line: the splice reproduces the new full data.
    let edited = "struct S { int v; };\nint f(S& s) { return s.v; }\nint g(S& tt) { return tt.v; }\nint h(int q) { return q; }\n";
    c.change(&a, edited);
    c.server.apply_pending();
    let r = c.request("textDocument/semanticTokens/full/delta", json!({"textDocument": doc, "previousResultId": prev}))["result"].clone();
    for e in r["edits"].as_array().unwrap() {
        let (start, del) = (e["start"].as_u64().unwrap() as usize, e["deleteCount"].as_u64().unwrap() as usize);
        data.splice(start..start + del, ints(&e["data"]));
    }
    let fresh = c.request("textDocument/semanticTokens/full", json!({"textDocument": doc}))["result"]["data"].clone();
    assert_eq!(data, ints(&fresh));

    // A stale or unknown id falls back to the full data.
    let r = c.request("textDocument/semanticTokens/full/delta", json!({"textDocument": doc, "previousResultId": "nope"}))["result"].clone();
    assert_eq!(ints(&r["data"]), ints(&fresh));
}

#[test]
fn apply_pending_reports_whether_anything_changed() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    std::fs::write(&a, "int a;\n").unwrap();
    common::write_cdb(&root, false);
    let mut c = Client::new(&root);
    c.open(&a, "int a;\n");
    c.server.apply_pending();
    assert!(!c.server.apply_pending(), "nothing pending");
    c.change(&a, "int a; int b;\n");
    assert!(c.server.apply_pending());
    assert!(!c.server.apply_pending());
}

#[test]
fn completion_over_rpc_and_unknown_method() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let txt = "struct Engine { int power; void start(); };\nvoid f(Engine e) { e.\n}\n";
    std::fs::write(&a, txt).unwrap();
    let mut c = Client::new(&root);
    c.open(&a, txt);
    let off = txt.find("e.\n").unwrap() + 2;
    let (line, character) = offset_to_position(txt, &LineIndex::new(txt), off as u32);
    let r = c.request("textDocument/completion", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": {"line": line, "character": character}}));
    let mut labels: Vec<&str> = r["result"]["items"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap()).collect();
    labels.sort();
    assert_eq!(labels, ["power", "start"]);

    let r = c.request("textDocument/notAThing", json!({}));
    assert_eq!(r["error"]["code"], -32601);
}

#[test]
fn requests_before_initialize_fail_cleanly_and_garbage_params_do_not_panic() {
    let mut s = Server::new(None);
    let mut out = Vec::new();
    s.handle(json!({"jsonrpc": "2.0", "id": 1, "method": "textDocument/definition", "params": {}}), &mut out);
    assert_eq!(out[0]["error"]["code"], -32002);
    out.clear();
    s.handle(json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": null}), &mut out);
    for m in ["textDocument/definition", "textDocument/references", "textDocument/completion", "textDocument/documentSymbol", "workspace/symbol"] {
        out.clear();
        s.handle(json!({"jsonrpc": "2.0", "id": 3, "method": m, "params": {"textDocument": {"uri": "file:///nope/x.cpp"}, "position": {"line": 5, "character": 5}, "query": 7}}), &mut out);
        assert!(out[0]["result"].is_null(), "{m}");
    }
}

/// Real-world checks against a Godot checkout at `C:/dev/godot` (with a
/// `compile_commands.json`). Opt in with
/// `cargo test --release --test lsp godot -- --ignored --nocapture`.
mod godot {
    use super::*;

    const ROOT: &str = "C:/dev/godot";

    fn open() -> (Client, String, String) {
        let cdb = format!("{ROOT}/compile_commands.json");
        assert!(std::path::Path::new(&cdb).is_file(), "{cdb} not found");
        let mut c = Client { server: Server::new(Some(cdb.into())), next: 1 };
        c.request("initialize", json!({"rootUri": path_to_uri(ROOT), "capabilities": {}}));
        c.server.apply_pending();
        let file = format!("{ROOT}/core/object/ref_counted.cpp");
        let text = std::fs::read_to_string(&file).unwrap();
        c.open(&file, &text);
        (c, file, text)
    }

    fn definition(c: &mut Client, file: &str, text: &str, off: usize) -> Value {
        let (line, character) = offset_to_position(text, &LineIndex::new(text), off as u32);
        c.request("textDocument/definition", json!({"textDocument": {"uri": path_to_uri(file)}, "position": {"line": line, "character": character}}))
    }

    #[test]
    #[ignore = "needs a Godot checkout"]
    fn godot_definitions_resolve_across_headers() {
        let (mut c, file, text) = open();
        let at = text.find("ClassDB::bind_method").unwrap();
        let r = definition(&mut c, &file, &text, at);
        assert!(r["result"][0]["uri"].as_str().unwrap().ends_with("class_db.h"), "{r}");

        // The base class resolves into another header.
        let hdr = format!("{ROOT}/core/object/ref_counted.h");
        let htext = std::fs::read_to_string(&hdr).unwrap();
        c.open(&hdr, &htext);
        let at = htext.find("class RefCounted : public Object").unwrap() + "class RefCounted : public ".len();
        let r = definition(&mut c, &hdr, &htext, at);
        assert!(r["result"].as_array().unwrap().iter().any(|l| l["uri"].as_str().unwrap().ends_with("object.h")), "{r}");

        // And after an edit through the incremental path.
        let edited = format!("{text}\nint appended_symbol;\n");
        c.change(&file, &edited);
        let r = definition(&mut c, &file, &edited, edited.find("ClassDB::bind_method").unwrap());
        assert!(r["result"][0]["uri"].as_str().unwrap().ends_with("class_db.h"), "{r}");
    }

    #[test]
    #[ignore = "needs a Godot checkout and --release"]
    fn godot_references_latency() {
        let (mut c, file, text) = open();
        for needle in ["ClassDB::bind_method", "RefCounted::reference", "RefCounted::unreference", "RefCounted::get_reference_count", "RefCounted::RefCounted", "init_ref"] {
            let at = text.find(needle).unwrap() + needle.find("::").map_or(0, |i| i + 2);
            let (line, character) = offset_to_position(&text, &LineIndex::new(&text), at as u32);
            let t0 = std::time::Instant::now();
            let r = c.request("textDocument/references", json!({"textDocument": {"uri": path_to_uri(&file)}, "position": {"line": line, "character": character}, "context": {"includeDeclaration": true}}));
            eprintln!("references {needle}: {} locations in {:?}", r["result"].as_array().map_or(0, Vec::len), t0.elapsed());
        }
    }

    #[test]
    #[ignore = "needs a Godot checkout and --release"]
    fn godot_typing_and_semantic_token_latency() {
        let (mut c, file, text) = open();
        let doc = json!({"uri": path_to_uri(&file)});
        let t0 = std::time::Instant::now();
        let r = c.request("textDocument/semanticTokens/full", json!({"textDocument": doc}));
        let dt = t0.elapsed();
        eprintln!("semantic tokens: {} tokens in {dt:?}", decode_tokens(&r).len());
        assert!(dt.as_millis() < 200, "semantic tokens: {dt:?}");

        // Every keystroke is a didChange followed by a query.
        let at = text.find("ClassDB::bind_method").unwrap();
        let ins = text[..at].rfind('\n').unwrap() + 1;
        let mut worst = std::time::Duration::ZERO;
        for i in 0..30 {
            let line = format!("int typed_{i};\n");
            let edited = format!("{}{line}{}", &text[..ins], &text[ins..]);
            let t0 = std::time::Instant::now();
            c.change(&file, &edited);
            c.server.apply_pending();
            let r = definition(&mut c, &file, &edited, at + line.len());
            worst = worst.max(t0.elapsed());
            assert!(r["result"][0]["uri"].as_str().unwrap().ends_with("class_db.h"), "iteration {i}: {r}");
        }
        eprintln!("typing: worst {worst:?}");
        assert!(worst.as_millis() < 100, "keystroke latency too high: {worst:?}");
    }
}

#[test]
fn definition_on_class_name_prefers_class_over_constructors() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let txt = "struct Base { Base(); ~Base(); };\nBase::Base() {}\nBase::~Base() {}\nstruct Derived : public Base {};\n";
    std::fs::write(&a, txt).unwrap();
    let mut c = Client::new(&root);
    c.open(&a, txt);
    let r = c.request("textDocument/definition", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": pos_of(txt, "Base {};", 0)}));
    let locs = r["result"].as_array().unwrap();
    assert_eq!(locs.len(), 1, "{r}");
    assert_eq!(locs[0]["range"]["start"]["line"], 0, "{r}");
}

#[test]
fn rename_class_members_and_refuse_outside_workspace_over_rpc() {
    let (_d, root) = tmp();
    let h = format!("{root}/shape.h");
    let a = format!("{root}/a.cpp");
    let htxt = "#pragma once\nstruct Shape { virtual float area(); int sides; };\nstruct Sq : Shape { float area() override; };\n";
    let atxt = "#include \"shape.h\"\nfloat Shape::area() { return sides; }\nfloat Sq::area() { return 4; }\nfloat use(Shape* s, Sq q) { return s->area() + q.area() + s->sides; }\n";
    std::fs::write(&h, htxt).unwrap();
    std::fs::write(&a, atxt).unwrap();
    common::write_cdb(&root, true);
    let mut c = Client::new(&root);
    c.open(&a, atxt);
    let rename = |c: &mut Client, pos: Value, name: &str| {
        c.request("textDocument/rename", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": pos, "newName": name}))
    };

    let r = rename(&mut c, pos_of(atxt, "area", 2), "surface");
    let changes = r["result"]["changes"].as_object().unwrap_or_else(|| panic!("{r}"));
    let count = |f: &str| changes.get(&path_to_uri(f)).map_or(0, |e| e.as_array().unwrap().len());
    assert_eq!(count(&h), 2, "{r}"); // Shape::area, Sq::area
    assert_eq!(count(&a), 4, "{r}"); // two definitions, two calls
    assert!(changes.values().flat_map(|e| e.as_array().unwrap()).all(|e| e["newText"] == "surface"));

    let r = rename(&mut c, pos_of(atxt, "sides", 0), "edges");
    assert_eq!(r["result"]["changes"][path_to_uri(&h)].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["result"]["changes"][path_to_uri(&a)].as_array().unwrap().len(), 2, "{r}");

    let r = rename(&mut c, pos_of(atxt, "sides", 0), "not valid");
    assert!(r["error"]["message"].as_str().unwrap().contains("not a valid identifier"), "{r}");
    let r = c.request("textDocument/prepareRename", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": pos_of(atxt, "area", 2)}));
    assert_eq!(r["result"]["placeholder"], "area", "{r}");
}

#[test]
fn rename_macros_and_enum_constants_over_rpc() {
    let (_d, root) = tmp();
    let h = format!("{root}/cfg.h");
    let a = format!("{root}/a.cpp");
    let htxt = "#pragma once\n#define LIMIT 8\n#define CLAMP(v) ((v) < LIMIT ? (v) : LIMIT)\nenum class Mode { Fast, Slow };\n";
    let atxt = "#include \"cfg.h\"\nint a[LIMIT];\nint f(int v) { return CLAMP(v); }\nMode m = Mode::Fast;\nint LIMITS = 1;\n";
    std::fs::write(&h, htxt).unwrap();
    std::fs::write(&a, atxt).unwrap();
    common::write_cdb(&root, true);
    let mut c = Client::new(&root);
    c.open(&a, atxt);
    let mut rename = |pos: Value, name: &str| {
        c.request("textDocument/rename", json!({"textDocument": {"uri": path_to_uri(&a)}, "position": pos, "newName": name}))
    };

    let r = rename(pos_of(atxt, "LIMIT", 0), "MAX_LIMIT");
    let ch = r["result"]["changes"].as_object().unwrap_or_else(|| panic!("{r}"));
    assert_eq!(ch[&path_to_uri(&h)].as_array().unwrap().len(), 3, "{r}"); // define + two uses in CLAMP
    assert_eq!(ch[&path_to_uri(&a)].as_array().unwrap().len(), 1, "{r}"); // `LIMITS` is a different identifier

    let r = rename(pos_of(atxt, "Fast", 0), "Quick");
    let ch = r["result"]["changes"].as_object().unwrap_or_else(|| panic!("{r}"));
    assert_eq!(ch[&path_to_uri(&h)].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(ch[&path_to_uri(&a)].as_array().unwrap().len(), 1, "{r}");
}

#[test]
fn parameters_navigate_and_rename_over_rpc() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let src = "struct Mod { void* find(const char* name) const; int name; };\n\
void* Mod::find(const char* name) const {\n  auto n = name;\n  return lookup(name, n);\n}\n";
    std::fs::write(&a, src).unwrap();
    common::write_cdb(&root, false);
    let mut c = Client::new(&root);
    c.open(&a, src);
    let doc = json!({"uri": path_to_uri(&a)});
    let at = |nth: usize| json!({"textDocument": doc, "position": pos_of(src, "name", nth)});

    // Hover on a parameter shows it as a parameter, not the same-named field.
    let r = c.request("textDocument/hover", at(2));
    let md = r["result"]["contents"]["value"].as_str().unwrap();
    assert!(md.contains("const char* name") && md.contains("parameter"), "{md}");

    // Definition of a use jumps to the parameter in the same definition (line 1), not the field.
    let r = c.request("textDocument/definition", at(4));
    assert_eq!(r["result"][0]["range"]["start"]["line"], 1, "{r}");
    // References and rename cover exactly this function's parameter.
    let r = c.request("textDocument/references", at(3));
    assert_eq!(r["result"].as_array().unwrap().len(), 3, "{r}");
    let r = c.request("textDocument/prepareRename", at(3));
    assert_eq!(r["result"]["placeholder"], "name");
    let mut p = at(3);
    p["newName"] = json!("key");
    let r = c.request("textDocument/rename", p);
    let edits = r["result"]["changes"][path_to_uri(&a)].as_array().unwrap();
    assert_eq!(edits.len(), 3, "{r}");
    assert!(edits.iter().all(|e| e["newText"] == "key"));
    // Renaming the same-named field leaves every parameter alone.
    let mut p = at(1);
    p["newName"] = json!("label");
    let r = c.request("textDocument/rename", p);
    assert_eq!(r["result"]["changes"][path_to_uri(&a)].as_array().unwrap().len(), 1, "{r}");
}

#[test]
fn missing_cdb_hint_falls_back_to_workspace_and_warns_when_none() {
    let (_d, root) = tmp();
    std::fs::write(format!("{root}/a.cpp"), "struct Found {};\n").unwrap();
    common::write_cdb(&root, false);
    let mut s = Server::new(Some("C:/path/to/nowhere.json".into()));
    let mut out = Vec::new();
    s.handle(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"rootUri": path_to_uri(&root)}}), &mut out);
    assert!(out.iter().any(|m| m["method"] == "window/showMessage"));
    assert_eq!(s.index().unwrap().db.lookup("Found").len(), 1);

    let (_e, empty) = tmp();
    let mut s = Server::new(None);
    out.clear();
    s.handle(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"rootUri": path_to_uri(&empty)}}), &mut out);
    assert!(out.iter().any(|m| m["method"] == "window/showMessage"));
}

#[test]
fn stdio_smoke_test() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    std::fs::write(&a, "struct Widget {};\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lightspeed"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    write_frame(&mut stdin, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"rootUri": path_to_uri(&root)}})).unwrap();
    let mut next = || -> Value {
        loop {
            let v: Value = serde_json::from_slice(&read_frame(&mut stdout).unwrap().unwrap()).unwrap();
            if v.get("id").is_some() {
                return v;
            }
        }
    };
    let init = next();
    assert!(init["result"]["capabilities"]["referencesProvider"].as_bool().unwrap());
    write_frame(&mut stdin, &json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": {"uri": path_to_uri(&a), "text": "struct Widget {};\n"}}})).unwrap();
    // Indexing runs in the background; requests answer empty until it lands.
    let mut r = Value::Null;
    for n in 0..200 {
        write_frame(&mut stdin, &json!({"jsonrpc": "2.0", "id": 100 + n, "method": "workspace/symbol", "params": {"query": "widget"}})).unwrap();
        r = next();
        assert!(r.get("error").is_none(), "requests during startup must not error: {r}");
        if r["result"][0]["name"] == "Widget" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(r["result"][0]["name"], "Widget", "{r}");
    write_frame(&mut stdin, &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": 7}})).unwrap();
    write_frame(&mut stdin, &json!({"jsonrpc": "2.0", "id": 7, "method": "workspace/symbol", "params": {"query": "widget"}})).unwrap();
    let r = next();
    assert_eq!(r["error"]["code"], -32800, "{r}");
    write_frame(&mut stdin, &json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown"})).unwrap();
    let _ = next();
    write_frame(&mut stdin, &json!({"jsonrpc": "2.0", "method": "exit"})).unwrap();
    stdin.flush().unwrap();
    let mut rest = String::new();
    let _ = stdout.read_to_string(&mut rest);
    assert!(child.wait().unwrap().success());
}
