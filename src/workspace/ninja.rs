//! Generated `.ninja` build files as a compile database.
//!
//! Ninja's own `-t compdb` is useless for our generated builds: the flags live in an
//! rspfile (`rspfile_content = $clflags $in`) that only exists after a build. Here the
//! rule is evaluated in memory instead, with ninja's scoping rules: `$in`/`$out`, then
//! the edge's bindings, then the rule's, then the file's top-level variables.
//!
//! Scope is deliberately our generator's output, not arbitrary ninja:
//! - a compile edge is `build <obj>: cl <source>` with a C/C++ source extension;
//! - `include`/`subninja`/`pool`/`default` are ignored;
//! - top-level variables are resolved once at the end of the file (last definition wins),
//!   so edge bindings see the final value of any global.
//!
//! Bulkbuild translation units (`BB_*.cpp` or `*_master[NN].cpp`) are replaced by
//! the `#include "member.cpp"` files they list, each compiled with the bulk edge's flags.
//! A member that also has an edge of its own keeps that edge's flags.

use crate::workspace::compile_commands::{FlagSet, absolutize, parse_args, split_command};
use hashbrown::HashSet;
use rayon::prelude::*;
use rustc_hash::{FxBuildHasher, FxHashMap};
use std::sync::Arc;

/// Rule name of C/C++ compiles in our generated files.
const COMPILE_RULE: &str = "cl";
/// Bulk files including bulk files; deeper nesting is treated as a cycle.
const MAX_BULK_DEPTH: u32 = 4;
const SOURCE_EXTS: &[&str] = &["c", "cc", "cpp", "cxx"];

type Parsed = (String, Arc<FlagSet>, u64);

#[derive(Default)]
struct Rule {
    bindings: Vec<(String, String)>,
}

struct Edge {
    rule: String,
    /// Explicit inputs, still containing `$` escapes.
    inputs: Vec<String>,
    outputs: Vec<String>,
    bindings: Vec<(String, String)>,
}

#[derive(Default)]
struct Ninja {
    globals: FxHashMap<String, String>,
    rules: FxHashMap<String, Rule>,
    edges: Vec<Edge>,
}

/// Expands `$name`, `${name}`, `$$`, `$ ` and `$:`; unknown variables are empty.
fn expand(s: &str, lookup: &mut dyn FnMut(&str) -> Option<String>) -> String {
    if !s.contains('$') {
        return s.to_owned();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            let start = i;
            while i < b.len() && b[i] != b'$' {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        i += 1;
        match b.get(i) {
            Some(b'$') => out.push('$'),
            Some(b' ') => out.push(' '),
            Some(b':') => out.push(':'),
            Some(b'{') => {
                if let Some(end) = s[i..].find('}') {
                    out.push_str(&lookup(&s[i + 1..i + end]).unwrap_or_default());
                    i += end;
                }
            }
            Some(c) if c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'-') {
                    i += 1;
                }
                out.push_str(&lookup(&s[start..i]).unwrap_or_default());
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Logical lines: `$`-newline continuations are joined (leading blanks of the next line dropped).
fn logical_lines(text: &str) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
    let mut raw = text.split('\n');
    std::iter::from_fn(move || {
        let first = raw.next()?.trim_end_matches('\r');
        let continues = |l: &str| l.len() - l.trim_end_matches('$').len() & 1 == 1;
        if !continues(first) {
            return Some(first.into());
        }
        let mut line = first[..first.len() - 1].to_owned();
        for next in raw.by_ref() {
            let next = next.trim_end_matches('\r');
            let more = continues(next);
            line.push_str(next.trim_start());
            if !more {
                break;
            }
            line.pop();
        }
        Some(line.into())
    })
}

/// Splits on unescaped spaces; `$ ` and `$:` stay escaped for [`expand`].
fn tokens(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut out, mut start, mut i) = (Vec::new(), None, 0);
    while i < b.len() {
        match b[i] {
            b'$' => {
                start.get_or_insert(i);
                i += 1;
            }
            b' ' | b'\t' => {
                if let Some(st) = start.take() {
                    out.push(&s[st..i]);
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
        i += 1;
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

/// Index of the first `:` that is not escaped as `$:`.
fn find_colon(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'$' => i += 1,
            b':' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_binding(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once('=')?;
    Some((name.trim(), value.trim()))
}

enum Block {
    None,
    Rule(String),
    Edge,
}

impl Ninja {
    fn read(text: &str) -> Self {
        let mut n = Self::default();
        let mut block = Block::None;
        for line in logical_lines(text) {
            let indented = line.starts_with([' ', '\t']);
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if indented {
                let Some((name, value)) = parse_binding(t) else { continue };
                match &block {
                    Block::Rule(r) => {
                        if let Some(rule) = n.rules.get_mut(r) {
                            rule.bindings.push((name.to_owned(), value.to_owned()));
                        }
                    }
                    Block::Edge => {
                        if let Some(e) = n.edges.last_mut() {
                            e.bindings.push((name.to_owned(), value.to_owned()));
                        }
                    }
                    Block::None => {}
                }
                continue;
            }
            block = Block::None;
            if let Some(name) = t.strip_prefix("rule ") {
                let name = name.trim().to_owned();
                n.rules.insert(name.clone(), Rule::default());
                block = Block::Rule(name);
            } else if let Some(rest) = t.strip_prefix("build ") {
                // Only compile edges are kept; everything else is skipped without allocating.
                if let Some(e) = parse_edge(rest) {
                    n.edges.push(e);
                    block = Block::Edge;
                }
            } else if let Some((name, value)) = parse_binding(t) {
                let v = expand(value, &mut |k| n.globals.get(k).cloned());
                n.globals.insert(name.to_owned(), v);
            }
        }
        n
    }
}

fn parse_edge(rest: &str) -> Option<Edge> {
    let colon = find_colon(rest)?;
    let rule_and_ins = rest[colon + 1..].trim_start();
    let (rule, ins) = rule_and_ins.split_once([' ', '\t']).unwrap_or((rule_and_ins, ""));
    if rule != COMPILE_RULE {
        return None;
    }
    let outputs = tokens(&rest[..colon]).into_iter().take_while(|t| *t != "|").map(str::to_owned).collect();
    let inputs = tokens(ins).into_iter().take_while(|t| *t != "|" && *t != "||").map(str::to_owned).collect();
    Some(Edge { rule: rule.to_owned(), inputs, outputs, bindings: Vec::new() })
}

/// Lexically resolves `.`/`..` (no filesystem access) in a `/`-separated absolute path.
fn clean(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for (i, part) in path.split('/').enumerate() {
        match part {
            "." => {}
            ".." if out.len() > 1 => {
                out.pop();
            }
            ".." => {}
            "" if i > 0 => {}
            p => out.push(p),
        }
    }
    out.join("/")
}

fn abs_path(dir: &str, p: &str) -> String {
    clean(&absolutize(dir, p))
}

struct Env<'a> {
    n: &'a Ninja,
    edge: &'a Edge,
    /// Edge bindings expanded against the file scope, as ninja does at parse time.
    expanded: Vec<(&'a str, String)>,
    input: String,
    output: String,
}

impl<'a> Env<'a> {
    fn new(n: &'a Ninja, edge: &'a Edge, input: &str) -> Self {
        let g = |s: &str| expand(s, &mut |k| n.globals.get(k).cloned());
        let expanded = edge.bindings.iter().map(|(k, v)| (k.as_str(), g(v))).collect();
        let quoted = if input.contains(' ') { format!("\"{input}\"") } else { input.to_owned() };
        Self { n, edge, expanded, input: quoted, output: edge.outputs.first().map(|o| g(o)).unwrap_or_default() }
    }

    fn lookup(&self, name: &str, depth: u32) -> Option<String> {
        match name {
            "in" => return Some(self.input.clone()),
            "out" => return Some(self.output.clone()),
            _ => {}
        }
        if let Some((_, v)) = self.expanded.iter().rev().find(|(k, _)| *k == name) {
            return Some(v.clone());
        }
        if depth > 0 && let Some(rule) = self.n.rules.get(&self.edge.rule) {
            if let Some((_, v)) = rule.bindings.iter().rev().find(|(k, _)| k == name) {
                return Some(expand(v, &mut |k| self.lookup(k, depth - 1)));
            }
        }
        self.n.globals.get(name).cloned()
    }

    fn eval(&self, name: &str) -> Option<String> {
        self.lookup(name, 8)
    }
}

/// argv of a compile edge: the compiler, then the flags from `rspfile_content` (or the command line).
fn argv(env: &Env) -> Vec<String> {
    let rsp = env.eval("rspfile_content").filter(|s| !s.trim().is_empty());
    let command = split_command(&env.eval("command").unwrap_or_default());
    match rsp {
        Some(flags) => command.into_iter().take(1).chain(split_command(&flags)).collect(),
        None => command,
    }
}

fn has_source_ext(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, e)| SOURCE_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Bulkbuild files come in two spellings: `BB_*.cpp` and `<Module>_master[NN].cpp`.
fn is_bulk_name(path: &str) -> bool {
    let name = base_name(path);
    let Some(stem) = name.rsplit_once('.').map(|(s, _)| s) else { return false };
    name.starts_with("BB_") || stem.trim_end_matches(|c: char| c.is_ascii_digit()).ends_with("_master")
}

/// Members of a bulkbuild file, or `None` if `path` is not a readable bulk file.
fn bulk_members(path: &str, depth: u32) -> Option<Vec<String>> {
    if depth > MAX_BULK_DEPTH || !is_bulk_name(path) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(inc) = line.trim_start().strip_prefix("#include") else { continue };
        let Some(inc) = inc.trim_start().strip_prefix('"') else { continue };
        let Some((member, _)) = inc.split_once('"') else { continue };
        let member = abs_path(dir, member);
        if !has_source_ext(&member) {
            continue;
        }
        match bulk_members(&member, depth + 1) {
            Some(nested) => out.extend(nested),
            None => out.push(member),
        }
    }
    // A file that includes no sources is just a regular source file.
    (!out.is_empty()).then_some(out)
}

/// Parses a generated `.ninja` file into `(file, flags, flags hash)` entries.
/// Relative paths resolve against the file's `basedir` variable when it has one (our
/// generator runs ninja from there), otherwise against `dir`, the file's own directory.
pub(crate) fn parse(text: &str, dir: &str) -> Vec<Parsed> {
    let n = Ninja::read(text);
    let dir = &n.globals.get("basedir").filter(|b| !b.is_empty()).map_or_else(|| dir.to_owned(), |b| abs_path(dir, b));
    // (entry, came from bulk expansion)
    let per_edge: Vec<Vec<(Parsed, bool)>> = n
        .edges
        .par_iter()
        .map(|edge| {
            let Some(raw) = edge.inputs.first() else { return Vec::new() };
            let src = abs_path(dir, &expand(raw, &mut |k| n.globals.get(k).cloned()));
            if !has_source_ext(&src) {
                return Vec::new();
            }
            let args = argv(&Env::new(&n, edge, &src));
            let flags = Arc::new(parse_args(&args, dir));
            let hash = flags.hash();
            match bulk_members(&src, 0) {
                Some(members) => members.into_iter().map(|m| ((m, flags.clone(), hash), true)).collect(),
                None => vec![((src, flags, hash), false)],
            }
        })
        .collect();
    // A member with an edge of its own keeps that edge's flags, whatever order they appear in.
    let direct: HashSet<&str, FxBuildHasher> =
        per_edge.iter().flatten().filter(|(_, bulk)| !bulk).map(|((f, ..), _)| f.as_str()).collect();
    let mut seen: HashSet<String, FxBuildHasher> = HashSet::default();
    let mut out = Vec::new();
    for ((file, flags, hash), bulk) in per_edge.iter().flatten() {
        if (*bulk && direct.contains(file.as_str())) || !seen.insert(file.clone()) {
            continue;
        }
        out.push((file.clone(), Arc::clone(flags), *hash));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::compile_commands::CompileDb;
    use std::fs;
    use std::path::Path;

    const PREAMBLE: &str = r#"
builddir = {root}/Build
clexe = C:/tools/cl.exe
copyexe = C:/tools/copy.exe
rule cl
  command = $clexe /showIncludes @"$rspfile"
  deps = msvc
  rspfile = $out.rsp
  rspfile_content = $clflags $in
rule copy
  command = $copyexe $in $out
Mod_cl = /W4 /std:c++20 /DMOD=1 /I "{root}/inc dir" /FI "pch.h"
Other_cl = /std:c11 /DOTHER
"#;

    fn p(root: &Path) -> String {
        root.to_string_lossy().replace('\\', "/")
    }

    fn bulk_file(root: &str, members: &[&str]) -> String {
        let mut s = String::from("#pragma warning(disable: 4206)\n");
        for m in members {
            s += &format!("#pragma optimize(\"\", on)\n#include \"{root}/src/{m}\"\n");
        }
        s
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: String,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = p(dir.path());
            fs::create_dir_all(dir.path().join("Build/gen")).unwrap();
            fs::create_dir_all(dir.path().join("src")).unwrap();
            Self { _dir: dir, root }
        }

        fn write(&self, rel: &str, text: &str) {
            let path = Path::new(&self.root).join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn db(&self, edges: &str) -> CompileDb {
            let text = format!("{}{edges}", PREAMBLE.replace("{root}", &self.root));
            let ninja = Path::new(&self.root).join("Build/gen/Tool.ninja");
            fs::write(&ninja, text).unwrap();
            CompileDb::load_ninja(&ninja).unwrap()
        }
    }

    fn macros(db: &CompileDb, file: &str) -> Vec<String> {
        let e = db.get(file).unwrap_or_else(|| panic!("no entry for {file}"));
        db.flags(e).macros.iter().map(|m| m.name.clone()).collect()
    }

    #[test]
    fn evaluates_rspfile_flags_for_a_plain_edge() {
        let fx = Fixture::new();
        let r = &fx.root;
        let db = fx.db(&format!(
            "build obj/a.obj: cl ../../src/a.cpp | pch.pch || deps\n  clflags = $Mod_cl /Fo\"$builddir/a.obj\"\n  rspfile = x.rsp\n"
        ));
        assert_eq!(db.entries.len(), 1);
        let e = db.get(&format!("{r}/src/a.cpp")).expect("`..` collapsed against the ninja directory");
        let f = db.flags(e);
        assert_eq!(f.compiler.as_deref(), Some("C:/tools/cl.exe"));
        assert_eq!(f.std.as_deref(), Some("c++20"));
        assert_eq!(macros(&db, &format!("{r}/src/a.cpp")), ["MOD"]);
        let dirs: Vec<_> = f.include_dirs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(dirs, [format!("{r}/inc dir")], "quoted path with a space stays one argument");
        assert_eq!(f.force_includes, [format!("{}/pch.h", p(Path::new(&fx.root).join("Build/gen").as_path()))]);
    }

    #[test]
    fn expands_a_bulkbuild_into_its_members_with_the_bulk_edges_flags() {
        let fx = Fixture::new();
        let r = fx.root.clone();
        fx.write("Build/gen/mod/BB_runtime.Mod_All.cpp", &bulk_file(&r, &["a.cpp", "sub/b.cpp", "c.c"]));
        let db = fx.db(&format!(
            "build mod/bb.obj: cl mod/BB_runtime.Mod_All.cpp || deps\n  clflags = $Mod_cl /Fo\"x.obj\"\n"
        ));
        let mut files: Vec<_> = db.entries.iter().map(|e| e.file.clone()).collect();
        files.sort();
        assert_eq!(files, [format!("{r}/src/a.cpp"), format!("{r}/src/c.c"), format!("{r}/src/sub/b.cpp")]);
        assert!(db.get(&format!("{r}/Build/gen/mod/BB_runtime.Mod_All.cpp")).is_none(), "bulk TU itself is replaced");
        for f in &files {
            assert_eq!(macros(&db, f), ["MOD"]);
        }
        let flags = db.entries.iter().map(|e| e.flags).collect::<hashbrown::HashSet<_>>();
        assert_eq!(flags.len(), 1, "all members share one flag set");
    }

    #[test]
    fn master_named_bulk_files_expand_too() {
        let fx = Fixture::new();
        let r = fx.root.clone();
        fx.write("Build/gen/mod/Engine.Mod_master02.cpp", &bulk_file(&r, &["a.cpp", "b.cpp"]));
        fx.write("Build/gen/mod/Engine.Mod_masterful.cpp", &bulk_file(&r, &["c.cpp"]));
        let db = fx.db(
            "build x.obj: cl mod/Engine.Mod_master02.cpp\n  clflags = $Mod_cl\nbuild y.obj: cl mod/Engine.Mod_masterful.cpp\n  clflags = $Mod_cl\n",
        );
        let mut files: Vec<_> = db.entries.iter().map(|e| e.file.clone()).collect();
        files.sort();
        assert_eq!(files, [format!("{r}/Build/gen/mod/Engine.Mod_masterful.cpp"), format!("{r}/src/a.cpp"), format!("{r}/src/b.cpp")]);
    }

    #[test]
    fn member_with_its_own_edge_keeps_its_own_flags_in_either_order() {
        let fx = Fixture::new();
        let r = fx.root.clone();
        fx.write("Build/gen/mod/BB_Mod_All.cpp", &bulk_file(&r, &["a.cpp", "b.cpp"]));
        let bulk = "build bb.obj: cl mod/BB_Mod_All.cpp\n  clflags = $Mod_cl\n";
        let own = format!("build a.obj: cl {r}/src/a.cpp\n  clflags = $Other_cl\n");
        for edges in [format!("{bulk}{own}"), format!("{own}{bulk}")] {
            let db = fx.db(&edges);
            assert_eq!(db.entries.len(), 2, "no duplicate entry for a.cpp");
            assert_eq!(macros(&db, &format!("{r}/src/a.cpp")), ["OTHER"]);
            assert_eq!(db.flags(db.get(&format!("{r}/src/a.cpp")).unwrap()).std.as_deref(), Some("c11"));
            assert_eq!(macros(&db, &format!("{r}/src/b.cpp")), ["MOD"]);
        }
    }

    #[test]
    fn nested_bulk_files_and_non_source_includes() {
        let fx = Fixture::new();
        let r = fx.root.clone();
        fx.write("src/BB_inner.cpp", &bulk_file(&r, &["x.cpp"]));
        let outer = format!(
            "#include \"{r}/src/BB_inner.cpp\"\n#include \"{r}/src/y.cxx\"\n#include \"{r}/src/z.h\"\n#include <vector>\n  #include   \"{r}/src/w.cc\"  // trailing\n"
        );
        fx.write("Build/gen/mod/BB_outer.cpp", &outer);
        let db = fx.db("build bb.obj: cl mod/BB_outer.cpp\n  clflags = $Mod_cl\n");
        let mut files: Vec<_> = db.entries.iter().map(|e| e.file.strip_prefix(&format!("{r}/src/")).unwrap().to_owned()).collect();
        files.sort();
        assert_eq!(files, ["w.cc", "x.cpp", "y.cxx"]);
    }

    #[test]
    fn unreadable_or_memberless_bulk_names_stay_ordinary_sources() {
        let fx = Fixture::new();
        let r = fx.root.clone();
        fx.write("Build/gen/mod/BB_plain.cpp", "// hand written, happens to start with BB_\nint x;\n");
        let db = fx.db(
            "build a.obj: cl mod/BB_plain.cpp\n  clflags = $Mod_cl\nbuild b.obj: cl mod/BB_missing.cpp\n  clflags = $Mod_cl\n",
        );
        let mut files: Vec<_> = db.entries.iter().map(|e| e.file.clone()).collect();
        files.sort();
        assert_eq!(files, [format!("{r}/Build/gen/mod/BB_missing.cpp"), format!("{r}/Build/gen/mod/BB_plain.cpp")]);
    }

    #[test]
    fn relative_paths_resolve_against_basedir_when_defined() {
        let fx = Fixture::new();
        let r = fx.root.clone();
        fx.write("Build/mod/BB_Mod_All.cpp", &bulk_file(&r, &["a.cpp"]));
        let db = fx.db(&format!(
            "basedir = {r}/Build\nbuild bb.obj: cl mod/BB_Mod_All.cpp\n  clflags = $Mod_cl\nbuild s.obj: cl ../src/s.cpp\n  clflags = $Mod_cl /I rel\n"
        ));
        assert!(db.get(&format!("{r}/src/a.cpp")).is_some(), "bulk file found under basedir, not the ninja dir");
        let s = db.get(&format!("{r}/src/s.cpp")).expect("`..` relative to basedir");
        assert_eq!(db.flags(s).include_dirs[1].path, format!("{r}/Build/rel"));
    }

    #[test]
    fn ignores_other_rules_and_non_source_inputs() {
        let fx = Fixture::new();
        let r = fx.root.clone();
        let db = fx.db(
            "build out/x.dll: copy in/x.dll\n\
             build y.pch: cl y.h\n  clflags = $Mod_cl\n\
             build z.obj: cl z.cpp\n  clflags = $Mod_cl\n",
        );
        assert_eq!(db.entries.len(), 1);
        assert!(db.get(&format!("{r}/Build/gen/z.cpp")).is_some());
    }

    #[test]
    fn variable_scoping_escapes_and_continuations() {
        let text = "\
a = one
b = ${a}-two ${a}x $$ $:
rule r
  command = tool $b $v
  rspfile_content = $v $in $out
build o$ 1.obj: r s$ p.cpp | imp
  v = edge $a
";
        let n = Ninja::read(text);
        assert_eq!(n.globals["b"], "one-two onex $ :");
        let e = parse_edge("o$ 1.obj: cl s$ p.cpp | imp").unwrap();
        assert_eq!((e.outputs, e.inputs), (vec!["o$ 1.obj".to_owned()], vec!["s$ p.cpp".to_owned()]));
        assert!(parse_edge("x: copy y").is_none());

        let joined: Vec<_> = logical_lines("a = 1 $\n    2 $\n  3\nb = 4\r\n").map(|l| l.into_owned()).collect();
        assert_eq!(joined, ["a = 1 2 3", "b = 4", ""]);

        // Edge binding shadows the global, rule bindings see edge bindings, `$in`/`$out` are bound.
        let mut n = Ninja::default();
        n.globals.insert("v".into(), "global".into());
        n.rules.insert("cl".into(), Rule { bindings: vec![("command".into(), "cc $v $in $out".into())] });
        let edge = Edge {
            rule: "cl".into(),
            inputs: vec!["s.cpp".into()],
            outputs: vec!["s.obj".into()],
            bindings: vec![("v".into(), "edge".into())],
        };
        assert_eq!(Env::new(&n, &edge, "s.cpp").eval("command").unwrap(), "cc edge s.cpp s.obj");
        assert_eq!(Env::new(&n, &edge, "my s.cpp").eval("command").unwrap(), "cc edge \"my s.cpp\" s.obj");
    }

    #[test]
    fn self_referencing_rule_binding_terminates() {
        let mut n = Ninja::default();
        n.rules.insert("cl".into(), Rule { bindings: vec![("command".into(), "cc $command".into())] });
        let edge = Edge { rule: "cl".into(), inputs: vec![], outputs: vec![], bindings: vec![] };
        assert!(Env::new(&n, &edge, "a.cpp").eval("command").unwrap().starts_with("cc"));
    }

    #[test]
    fn cleans_paths_lexically() {
        assert_eq!(clean("E:/a/b/../../c/./d"), "E:/c/d");
        assert_eq!(clean("/a/../../b"), "/b");
        assert_eq!(abs_path("/proj/Build", "../src/a.cpp"), "/proj/src/a.cpp");
        assert_eq!(abs_path("/proj/Build", "/abs//x.cpp"), "/abs/x.cpp");
    }
}
