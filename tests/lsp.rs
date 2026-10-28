//! Protocol-level tests: drive the server with JSON-RPC messages.

use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};

use lightspeed::lsp::{Server, offset_to_position, path_to_uri, position_to_offset, read_frame, uri_to_path, write_frame};
use lightspeed::source::LineIndex;
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
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp", "-I", root]}])).unwrap(),
    )
    .unwrap();

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

#[test]
fn type_hierarchy_and_inheritance_graph_over_rpc() {
    let (_d, root) = tmp();
    let a = format!("{root}/a.cpp");
    let txt = "struct A {};\nstruct B : A {};\nstruct C : B {};\nstruct D : B {};\nstruct Other {};\nB* make();\n";
    std::fs::write(&a, txt).unwrap();
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp"]}])).unwrap(),
    )
    .unwrap();
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
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp"]}])).unwrap(),
    )
    .unwrap();
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
    let legend = ["namespace", "class", "struct", "enum", "enumMember", "function", "method", "property", "macro", "type", "variable"];
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
    let txt = "#define LIMIT 4\nnamespace N { struct S { static int count; virtual void run() = 0; int v; }; enum E { One }; }\nint f(N::S& s) {\n  /* é */ int x = LIMIT + N::One;\n  s.run(); s.v = f(s); return N::S::count;\n}\n";
    std::fs::write(&a, txt).unwrap();
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp"]}])).unwrap(),
    )
    .unwrap();
    let mut c = Client::new(&root);
    let caps = c.request("initialize", json!({"rootUri": path_to_uri(&root), "capabilities": {}}));
    assert_eq!(caps["result"]["capabilities"]["semanticTokensProvider"]["legend"]["tokenTypes"][5], "function");
    c.open(&a, txt);
    let doc = json!({"uri": path_to_uri(&a)});
    let all = decode_tokens(&c.request("textDocument/semanticTokens/full", json!({"textDocument": doc})));
    let find = |line: u32, col: u32| all.iter().find(|t| t.0 == line && t.1 == col).unwrap_or_else(|| panic!("no token at {line}:{col}: {all:?}"));
    assert_eq!(find(1, 10).3, "namespace");
    assert_eq!(find(1, 21).3, "struct");
    assert_eq!((find(1, 36).3.as_str(), find(1, 36).4), ("variable", 1)); // static data member `count`
    assert_eq!((find(1, 56).3.as_str(), find(1, 56).4), ("method", 4)); // pure virtual run
    assert_eq!(find(3, 18).3, "macro"); // after the multi-byte comment, still the right UTF-16 column
    assert_eq!(find(3, 29).3, "enumMember");
    assert_eq!(find(4, 4).3, "method");
    assert_eq!(find(4, 13).3, "property");
    assert_eq!(find(4, 17).3, "function");
    assert!(all.windows(2).all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)), "tokens must be ordered");

    let part = decode_tokens(&c.request("textDocument/semanticTokens/range", json!({"textDocument": doc, "range": {"start": {"line": 4, "character": 0}, "end": {"line": 5, "character": 0}}})));
    assert!(!part.is_empty() && part.iter().all(|t| t.0 == 4), "{part:?}");
}

/// Full-document latency on real files. Run with `cargo test --release --test lsp semantic_tokens_real -- --nocapture`.
#[test]
fn semantic_tokens_real_world_timing() {
    let root = "C:/dev/shiki";
    let cdb = format!("{root}/compile_commands.json");
    if !std::path::Path::new(&cdb).is_file() {
        return;
    }
    let mut c = Client { server: Server::new(Some(cdb.into())), next: 1 };
    c.request("initialize", json!({"rootUri": path_to_uri(root), "capabilities": {}}));
    c.server.apply_pending();
    let mut files: Vec<(u64, String)> = ["runtime/coff_module.cpp", "runtime/stress.cpp", "src/node_editor.cpp"]
        .iter()
        .map(|f| format!("{root}/{f}"))
        .chain(std::fs::read_dir("C:/Program Files (x86)/Windows Kits/10/Include").ok().and_then(|d| d.flatten().next()).map(|v| format!("{}/um/winnt.h", v.path().display().to_string().replace('\\', "/"))))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.len(), p)))
        .collect();
    files.sort();
    for (len, file) in files {
        let text = std::fs::read_to_string(&file).unwrap();
        c.open(&file, &text);
        let doc = json!({"uri": path_to_uri(&file)});
        let t0 = std::time::Instant::now();
        let r = c.request("textDocument/semanticTokens/full", json!({"textDocument": doc}));
        let dt = t0.elapsed();
        let n = decode_tokens(&r).len();
        eprintln!("semantic tokens: {file} ({len} bytes): {n} tokens in {dt:?}");
        assert!(dt.as_millis() < 200, "{file}: {dt:?}");
    }
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

/// Real-world regression: needs the shiki checkout and a Windows SDK; skipped elsewhere.
#[test]
fn real_world_coff_module_symbols_resolve_over_rpc() {
    let root = "C:/dev/shiki";
    let file = format!("{root}/runtime/coff_module.cpp");
    let cdb = format!("{root}/compile_commands.json");
    if !std::path::Path::new(&cdb).is_file() || !std::path::Path::new(&file).is_file() {
        return;
    }
    let mut c = Client { server: Server::new(Some(cdb.into())), next: 1 };
    c.request("initialize", json!({"rootUri": path_to_uri(root), "capabilities": {}}));
    let text = std::fs::read_to_string(&file).unwrap();
    // VS Code sends a lower-case drive letter.
    let uri = path_to_uri(&file).replacen("C:", "c%3A", 1);
    c.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": "cpp", "version": 1, "text": text}}));
    let check = |c: &mut Client, text: &str, uri: &str| {
        for name in ["IMAGE_SECTION_HEADER", "IMAGE_RELOCATION", "IMAGE_FILE_HEADER"] {
            for (n, _) in text.match_indices(name) {
                let (line, character) = offset_to_position(text, &LineIndex::new(text), n as u32 + 2);
                let r = c.request("textDocument/definition", json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}));
                let locs = r["result"].as_array().unwrap_or_else(|| panic!("{name}: {r}"));
                assert!(!locs.is_empty(), "{name} at line {line}: no definition");
            }
        }
    };
    check(&mut c, &text, &uri);
    // After an edit the file is re-indexed through the incremental path.
    let edited = format!("{text}\nint appended_symbol;\n");
    c.change(&file, &edited);
    c.notify("textDocument/didChange", json!({"textDocument": {"uri": uri}, "contentChanges": [{"text": edited}]}));
    check(&mut c, &edited, &uri);
}

/// Typing latency on a real file: every keystroke is a didChange followed by a query.
/// Run with `cargo test --release --test lsp typing_latency -- --nocapture`.
#[test]
fn typing_latency_real_world() {
    let root = "C:/dev/shiki";
    let file = format!("{root}/runtime/coff_module.cpp");
    let cdb = format!("{root}/compile_commands.json");
    if !std::path::Path::new(&cdb).is_file() || !std::path::Path::new(&file).is_file() {
        return;
    }
    let mut c = Client { server: Server::new(Some(cdb.into())), next: 1 };
    c.request("initialize", json!({"rootUri": path_to_uri(root), "capabilities": {}}));
    let text = std::fs::read_to_string(&file).unwrap();
    c.open(&file, &text);
    let uri = path_to_uri(&file);
    let at = text.find("relocation.Type").unwrap() + "relocation.".len();
    // Typing happens in the body, below the include prologue.
    let ins = text.match_indices("#include").last().map(|(i, _)| i + text[i..].find('\n').unwrap() + 1).unwrap();
    let mut worst = std::time::Duration::ZERO;
    let mut total = std::time::Duration::ZERO;
    const N: usize = 30;
    for i in 0..N {
        let line = format!("int typed_{i};\n");
        let edited = format!("{}{line}{}", &text[..ins], &text[ins..]);
        let t0 = std::time::Instant::now();
        c.change(&file, &edited);
        c.server.apply_pending();
        let t_apply = t0.elapsed();
        let off = at + line.len();
        let (line, character) = offset_to_position(&edited, &LineIndex::new(&edited), off as u32);
        let r = c.request("textDocument/definition", json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}));
        let dt = t0.elapsed();
        if i % 10 == 0 {
            eprintln!("  apply {t_apply:?}, query {:?}", dt - t_apply);
        }
        assert_eq!(r["result"].as_array().map(Vec::len), Some(1), "iteration {i}: {r}");
        assert!(r["result"][0]["uri"].as_str().unwrap().ends_with("winnt.h"));
        worst = worst.max(dt);
        total += dt;
    }
    eprintln!("typing: avg {:?}, worst {:?}", total / N as u32, worst);
    assert!(worst.as_millis() < 100, "keystroke latency too high: {worst:?}");
}

/// Half-typed code must not make the rest of the file un-navigable.
#[test]
fn half_typed_code_keeps_rest_of_file_navigable() {
    let root = "C:/dev/shiki";
    let file = format!("{root}/runtime/coff_module.cpp");
    let cdb = format!("{root}/compile_commands.json");
    if !std::path::Path::new(&cdb).is_file() || !std::path::Path::new(&file).is_file() {
        return;
    }
    let mut c = Client { server: Server::new(Some(cdb.into())), next: 1 };
    c.request("initialize", json!({"rootUri": path_to_uri(root), "capabilities": {}}));
    let text = std::fs::read_to_string(&file).unwrap();
    c.open(&file, &text);
    let uri = path_to_uri(&file);
    // Just inside the body of section_name, with plenty of code below it.
    let sig = text.find("static std::string section_name").unwrap();
    let ins = sig + text[sig..].find('{').unwrap() + 1;
    let garbage = ["x.", "x->", "foo(", "if (", "{", "}", "\"abc", "#if", "case", "std::", "template<", "relocation.", "switch (", "for (int i = 0;", "else", "'"];
    let mut failures = Vec::new();
    for g in garbage {
        let edited = format!("{}\n    {g}{}", &text[..ins], &text[ins..]);
        c.change(&file, &edited);
        // Declarations well below the edit must still resolve.
        let tail_start = ins + g.len() + 200;
        let mut checked = 0;
        for (n, _) in edited.match_indices("IMAGE_").filter(|(n, _)| *n > tail_start && edited[*n..].starts_with("IMAGE_SECTION_HEADER")) {
            let (line, character) = offset_to_position(&edited, &LineIndex::new(&edited), n as u32 + 2);
            let r = c.request("textDocument/definition", json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}));
            if r["result"].as_array().is_none_or(|a| a.is_empty()) {
                failures.push(format!("{g:?}: no definition at line {line}"));
                break;
            }
            checked += 1;
        }
        assert!(checked > 0 || !failures.is_empty(), "test found nothing to check for {g:?}");
    }
    assert!(failures.is_empty(), "{failures:#?}");
    // And once the code is complete again everything is back to normal.
    c.change(&file, &text);
    let n = text.find("relocation.Type").unwrap() + "relocation.".len();
    let (line, character) = offset_to_position(&text, &LineIndex::new(&text), n as u32);
    let r = c.request("textDocument/definition", json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}));
    assert_eq!(r["result"].as_array().map(Vec::len), Some(1));
}

/// Edits that move declarations without changing them (comments, bodies) are
/// patched in place: no index or hierarchy rebuild, and every offset stays right.
#[test]
fn body_edits_patch_in_place_and_keep_offsets_right() {
    let root = "C:/dev/shiki";
    let file = format!("{root}/runtime/coff_module.cpp");
    let cdb = format!("{root}/compile_commands.json");
    if !std::path::Path::new(&cdb).is_file() || !std::path::Path::new(&file).is_file() {
        return;
    }
    let mut c = Client { server: Server::new(Some(cdb.into())), next: 1 };
    c.request("initialize", json!({"rootUri": path_to_uri(root), "capabilities": {}}));
    let text = std::fs::read_to_string(&file).unwrap();
    c.open(&file, &text);
    let uri = path_to_uri(&file);
    let ins = text.find("relocation.Type").unwrap();
    let mut total = std::time::Duration::ZERO;
    for i in 0..20 {
        let pad = format!("/* edit {i} */\n    ").repeat(i + 1);
        let edited = format!("{}{pad}{}", &text[..ins], &text[ins..]);
        let t0 = std::time::Instant::now();
        c.change(&file, &edited);
        c.server.apply_pending();
        total += t0.elapsed();
        assert!(!c.server.index().unwrap().is_dirty());
        // The symbol the cursor sits on resolves from its *moved* position.
        let n = ins + pad.len() + "relocation.".len();
        let (line, character) = offset_to_position(&edited, &LineIndex::new(&edited), n as u32);
        let r = c.request("textDocument/definition", json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}));
        assert!(r["result"][0]["uri"].as_str().unwrap().ends_with("winnt.h"), "edit {i}: {r}");
        // The enclosing function's own definition points at its new line.
        let sym = edited.find("void apply_relocation").unwrap() + "void ".len();
        let (line, character) = offset_to_position(&edited, &LineIndex::new(&edited), sym as u32);
        let r = c.request("textDocument/definition", json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}));
        assert_eq!(r["result"][0]["range"]["start"]["line"], line, "edit {i}: {r}");
    }
    eprintln!("body edits: avg apply {:?}", total / 20);
}

#[test]
fn real_world_member_completion_after_dot() {
    let root = "C:/dev/shiki";
    let file = format!("{root}/runtime/coff_module.cpp");
    let cdb = format!("{root}/compile_commands.json");
    if !std::path::Path::new(&cdb).is_file() || !std::path::Path::new(&file).is_file() {
        return;
    }
    let mut c = Client { server: Server::new(Some(cdb.into())), next: 1 };
    c.request("initialize", json!({"rootUri": path_to_uri(root), "capabilities": {}}));
    let text = std::fs::read_to_string(&file).unwrap();
    // `relocation.` typed with nothing after it, as when the popup opens.
    let at = text.find("switch (relocation.Type)").unwrap() + "switch (relocation.".len();
    let edited = format!("{}{}", &text[..at], &text[at + "Type".len()..]);
    c.open(&file, &edited);
    let (line, character) = offset_to_position(&edited, &LineIndex::new(&edited), at as u32);
    let r = c.request("textDocument/completion", json!({"textDocument": {"uri": path_to_uri(&file)}, "position": {"line": line, "character": character}}));
    let labels: Vec<&str> = r["result"]["items"].as_array().unwrap().iter().filter_map(|i| i["label"].as_str()).collect();
    eprintln!("{labels:?}");
    for want in ["Type", "VirtualAddress", "SymbolTableIndex"] {
        assert!(labels.contains(&want), "missing {want}: {labels:?}");
    }
    assert!(labels.len() < 10, "should only list members of _IMAGE_RELOCATION: {labels:?}");
    let detail = |l: &str| r["result"]["items"].as_array().unwrap().iter().find(|i| i["label"] == l).and_then(|i| i["detail"].as_str()).unwrap_or("").to_owned();
    assert!(detail("Type").contains("Type"), "{}", detail("Type"));
    assert!(detail("VirtualAddress").contains("DWORD") || detail("VirtualAddress").contains("ULONG"), "{}", detail("VirtualAddress"));

    // Hovering `relocation.Type` shows the real field, its type and header.
    let at = text.find("switch (relocation.Type)").unwrap() + "switch (relocation.".len() + 1;
    c.open(&file, &text);
    let (line, character) = offset_to_position(&text, &LineIndex::new(&text), at as u32);
    let r = c.request("textDocument/hover", json!({"textDocument": {"uri": path_to_uri(&file)}, "position": {"line": line, "character": character}}));
    let md = r["result"]["contents"]["value"].as_str().unwrap_or_default();
    eprintln!("{md}");
    assert!(md.contains("Type") && md.contains("_IMAGE_RELOCATION") && md.contains("winnt.h"), "{md}");

    // A parameter in a real header hovers as a parameter.
    let hdr = format!("{root}/runtime/coff_module.h");
    let htext = std::fs::read_to_string(&hdr).unwrap();
    let at = htext.find("find(const std::string& name)").unwrap() + "find(const std::string& ".len() + 1;
    c.open(&hdr, &htext);
    let (line, character) = offset_to_position(&htext, &LineIndex::new(&htext), at as u32);
    let r = c.request("textDocument/hover", json!({"textDocument": {"uri": path_to_uri(&hdr)}, "position": {"line": line, "character": character}}));
    let md = r["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(md.contains("const std::string& name") && md.contains("parameter"), "{md}");
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
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp", "-I", root]}])).unwrap(),
    )
    .unwrap();
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
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp", "-I", root]}])).unwrap(),
    )
    .unwrap();
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
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp"]}])).unwrap(),
    )
    .unwrap();
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
    std::fs::write(
        format!("{root}/compile_commands.json"),
        serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp"]}])).unwrap(),
    )
    .unwrap();
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
