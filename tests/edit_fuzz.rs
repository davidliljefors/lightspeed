//! Randomised edit storms on real code bases (EASTL, SDL, with the MSVC and
//! Windows SDK headers when installed). Every few edits the incrementally
//! patched index is compared with a full rebuild of the same symbol tables, so
//! any state the patching path gets wrong shows up as a diff.
//!
//! Knobs: `LIGHTSPEED_FUZZ_ITERS` (edits per corpus, default 80),
//! `LIGHTSPEED_FUZZ_SEED`, `LIGHTSPEED_FUZZ_CHECK_EVERY` (default 4).
//! Corpora that are not checked out are skipped.

mod common;

use common::{Rng, observable};
use lightspeed::compile_commands::CompileDb;
use lightspeed::index::{Index, IndexConfig};
use std::collections::HashMap;
use std::path::Path;

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// MSVC, UCRT and SDK include directories, if installed.
fn system_includes() -> Vec<String> {
    let mut out = Vec::new();
    let sub = |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).collect::<Vec<_>>();
    for v in sub(Path::new(r"C:\Program Files\Microsoft Visual Studio")) {
        for e in sub(&v) {
            for m in sub(&e.join(r"VC\Tools\MSVC")) {
                if m.join("include").join("vector").exists() {
                    out.push(m.join("include"));
                }
            }
        }
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\Include");
    if let Some(ver) = sub(kits).into_iter().filter(|v| v.join("ucrt").exists()).max() {
        for d in ["ucrt", "shared", "um"] {
            out.push(ver.join(d));
        }
    }
    out.into_iter().take(4).map(|p| p.to_string_lossy().replace('\\', "/")).collect()
}

const MSVC_DEFS: [&str; 5] = ["-D_MSC_VER=1944", "-D_WIN64", "-D_M_X64", "-D_CPPLIB_VER=650", "-D_WIN32"];

fn json_str(s: &str) -> String {
    format!("\"{s}\"")
}

fn eastl() -> Option<Index> {
    let p = Path::new(r"C:\dev\EASTL\compile_commands.json");
    if !p.exists() {
        return None;
    }
    Some(Index::build(CompileDb::load(p).ok()?, IndexConfig::default()))
}

fn sdl() -> Option<Index> {
    let root = Path::new(r"C:\dev\SDL");
    if !root.join("include").exists() {
        return None;
    }
    let mut files = Vec::new();
    let mut stack = vec![root.join("src")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                let n = p.file_name().unwrap().to_string_lossy().to_lowercase();
                if !["android", "emscripten", "haiku", "ios", "n3ds", "ps2", "psp", "vita", "wayland", "x11", "kmsdrm", "cocoa", "uikit", "linux", "unix", "riscos", "os2", "nacl"].contains(&n.as_str()) {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "c") {
                files.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    files.truncate(200);
    let mut args = vec![json_str("clang"), json_str("-I"), json_str("C:/dev/SDL/include"), json_str("-I"), json_str("C:/dev/SDL/src")];
    for d in MSVC_DEFS {
        args.push(json_str(d));
    }
    for i in system_includes() {
        args.push(json_str("-I"));
        args.push(json_str(&i));
    }
    let entries: Vec<String> = files.iter().map(|f| format!(r#"{{"directory":"C:/dev/SDL","file":"{f}","arguments":[{}]}}"#, args.join(","))).collect();
    Some(Index::build(CompileDb::from_json(&format!("[{}]", entries.join(","))).ok()?, IndexConfig::default()))
}

/// A synthetic TU over a spread of system headers on top of EASTL-style classes.
fn stl() -> Option<Index> {
    let inc = system_includes();
    let first = inc.first()?;
    let ws = std::env::temp_dir().join("lightspeed_fuzz_stl");
    std::fs::create_dir_all(&ws).ok()?;
    let wsd = ws.to_string_lossy().replace('\\', "/");
    let mut entries = Vec::new();
    for (i, hdrs) in [["vector", "map", "unordered_map", "string"], ["memory", "functional", "algorithm", "optional"], ["variant", "tuple", "array", "list"]].iter().enumerate() {
        let src: String = hdrs.iter().map(|h| format!("#include <{h}>\n")).collect::<String>() + "struct Base { virtual void f(); virtual ~Base(); };\nstruct D : Base { void f() override; };\nvoid Base::f() {}\nvoid D::f() {}\n";
        let file = format!("{wsd}/t{i}.cpp");
        std::fs::write(&file, src).ok()?;
        let mut args = vec![json_str("clang++"), json_str("-std=c++17"), json_str("-I"), json_str(first)];
        for d in MSVC_DEFS {
            args.push(json_str(d));
        }
        entries.push(format!(r#"{{"directory":"{wsd}","file":"{file}","arguments":[{}]}}"#, args.join(",")));
    }
    Some(Index::build(CompileDb::from_json(&format!("[{}]", entries.join(","))).ok()?, IndexConfig::default()))
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn line_starts(s: &str) -> Vec<usize> {
    std::iter::once(0).chain(s.match_indices('\n').map(|(i, _)| i + 1)).filter(|&i| i <= s.len()).collect()
}

/// A random, usually structure-breaking, edit of `text`. Returns what it did.
fn mutate(rng: &mut Rng, text: &mut String, original: &str, classes: &[String], serial: usize) -> String {
    let b = text.as_bytes();
    let ls = line_starts(text);
    match rng.below(14) {
        0 => {
            // A newline inside an identifier.
            let start = rng.below(b.len().max(1));
            if let Some(i) = (start..b.len().saturating_sub(1)).find(|&i| is_ident(b[i]) && is_ident(b[i + 1])) {
                text.insert(i + 1, '\n');
                return format!("newline inside identifier at {}", i + 1);
            }
            "noop".into()
        }
        1 => {
            let l = rng.below(ls.len());
            let end = ls.get(l + 1).copied().unwrap_or(text.len());
            text.replace_range(ls[l]..end, "");
            format!("delete line {l}")
        }
        2 => {
            let l = rng.below(ls.len());
            let end = ls.get(l + 1).copied().unwrap_or(text.len());
            let dup = text[ls[l]..end].to_owned();
            text.insert_str(end, &dup);
            format!("duplicate line {l}")
        }
        3 => {
            let l = rng.below(ls.len());
            let n = 1 + rng.below(40);
            let end = ls.get(l + n).copied().unwrap_or(text.len());
            text.replace_range(ls[l]..end, "");
            format!("delete {n} lines from {l}")
        }
        4 => {
            let from = rng.below(text.len().max(1));
            match text[boundary(text, from)..].find("virtual ") {
                Some(i) => {
                    let at = boundary(text, from) + i;
                    text.replace_range(at..at + 8, "");
                    format!("drop virtual at {at}")
                }
                None => "noop".into(),
            }
        }
        5 => {
            let from = boundary(text, rng.below(text.len().max(1)));
            match text[from..].find("override") {
                Some(i) => {
                    text.replace_range(from + i..from + i + 8, "");
                    format!("drop override at {}", from + i)
                }
                None => "noop".into(),
            }
        }
        6 => {
            let from = boundary(text, rng.below(text.len().max(1)));
            match text[from..].find("void ") {
                Some(i) => {
                    text.insert_str(from + i, "virtual ");
                    format!("add virtual at {}", from + i)
                }
                None => "noop".into(),
            }
        }
        7 => {
            // Remove a base-specifier list.
            let from = boundary(text, rng.below(text.len().max(1)));
            let hit = ["struct ", "class "].iter().filter_map(|k| text[from..].find(k)).min();
            if let Some(i) = hit {
                let at = from + i;
                if let Some(colon) = text[at..].find(" : ").filter(|&c| c < 80)
                    && let Some(brace) = text[at + colon..].find('{')
                    && text[at..at + colon].find(['{', ';']).is_none()
                {
                    text.replace_range(at + colon..at + colon + brace, " ");
                    return format!("remove base clause at {at}");
                }
            }
            "noop".into()
        }
        8 => {
            let from = boundary(text, rng.below(text.len().max(1)));
            let cut = ["struct ", "class "].iter().filter_map(|k| text[from..].find(k)).min();
            match cut {
                Some(i) => {
                    let at = from + i + 7;
                    text.insert_str(at.min(text.len()), "Renamed");
                    format!("rename class at {at}")
                }
                None => "noop".into(),
            }
        }
        9 => {
            // A new class deriving from some existing one, with an overriding method.
            let base = classes.get(rng.below(classes.len().max(1))).cloned().unwrap_or_else(|| "Base".into());
            let at = ls[rng.below(ls.len())];
            let m = format!("method{}", rng.below(4));
            text.insert_str(at, &format!("struct Fuzz{serial} : {base} {{ virtual void {m}(); void {m}() override; }};\nvoid Fuzz{serial}::{m}() {{}}\n"));
            format!("insert class Fuzz{serial} : {base}")
        }
        10 => {
            let at = boundary(text, rng.below(text.len().max(1)));
            text.truncate(at);
            format!("truncate at {at}")
        }
        11 => {
            *text = original.to_owned();
            "revert to original".into()
        }
        12 => {
            let at = ls[rng.below(ls.len())];
            text.insert_str(at, "}\n");
            format!("stray brace at {at}")
        }
        _ => {
            let at = ls[rng.below(ls.len())];
            text.insert_str(at, &format!("#if 0\nstruct Hidden{serial} {{}};\n#endif\nint added{serial};\n"));
            format!("insert inactive block + variable at {at}")
        }
    }
}

fn class_names(text: &str, out: &mut Vec<String>) {
    for k in ["struct ", "class "] {
        for (i, _) in text.match_indices(k).take(8) {
            let rest = &text[i + k.len()..];
            let n: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if n.len() > 1 {
                out.push(n);
            }
        }
    }
}

fn diff(a: &[String], b: &[String]) -> (Vec<String>, Vec<String>) {
    let (sa, sb): (std::collections::HashSet<&String>, std::collections::HashSet<&String>) = (a.iter().collect(), b.iter().collect());
    (a.iter().filter(|x| !sb.contains(x)).take(5).cloned().collect(), b.iter().filter(|x| !sa.contains(x)).take(5).cloned().collect())
}

fn storm(name: &str, idx: Index) {
    let (iters, seed, every) = (env_num("LIGHTSPEED_FUZZ_ITERS", 80), env_num("LIGHTSPEED_FUZZ_SEED", 0x9E3779B97F4A7C15), env_num("LIGHTSPEED_FUZZ_CHECK_EVERY", 4));
    storm_with(name, idx, iters, seed, every);
}

fn storm_with(name: &str, mut idx: Index, iters: u64, seed: u64, every: u64) {
    let mut rng = Rng(seed);
    let files: Vec<String> = idx.live_files().map(|(_, f)| f.path.clone()).collect();
    assert!(!files.is_empty(), "{name}: nothing indexed");
    let mut texts: HashMap<String, (String, String)> = HashMap::new();
    let mut classes = Vec::new();
    for p in files.iter().step_by((files.len() / 150).max(1)) {
        if let Ok(t) = std::fs::read_to_string(p) {
            class_names(&t, &mut classes);
        }
    }
    eprintln!("{name}: {} files, {} symbols, seed {seed:#x}", files.len(), idx.db.len());
    let (mut log, mut checked, mut dirty_skips, mut patched_clean) = (Vec::<String>::new(), 0, 0, 0);
    for it in 0..iters {
        let path = &files[rng.below(files.len())];
        let Some(entry) = texts.entry(path.clone()).or_insert_with(|| {
            let t = std::fs::read_to_string(path).unwrap_or_default();
            (t.clone(), t)
        }).into() else { unreachable!() };
        if entry.0.is_empty() && entry.1.is_empty() {
            continue;
        }
        let what = mutate(&mut rng, &mut entry.0, &entry.1, &classes, it as usize);
        log.push(format!("#{it} {path}: {what}"));
        let text = entry.0.clone();
        idx.update(path, Some(&text));
        if (it + 1) % every != 0 {
            continue;
        }
        if idx.is_dirty() {
            dirty_skips += 1;
            idx.flush();
            continue;
        }
        patched_clean += 1;
        let patched = observable(&idx, 3);
        idx.rebuild_all();
        let full = observable(&idx, 3);
        checked += 1;
        if patched != full {
            let (only_patched, only_full) = diff(&patched, &full);
            let tail = log[log.len().saturating_sub(every as usize + 2)..].join("\n");
            panic!("{name}: patched index differs from a full rebuild after edit #{it}\nrecent edits:\n{tail}\npatched only: {only_patched:#?}\nfull rebuild only: {only_full:#?}");
        }
    }
    eprintln!("{name}: {checked} comparisons ok, {patched_clean} checks on a clean patched index, {dirty_skips} fell back to a full rebuild");
    assert!(checked > 0, "{name}: every check fell back to a full rebuild");
    assert!(dirty_skips <= checked, "{name}: too many full rebuilds ({dirty_skips} vs {checked} patched)");
}

#[test]
fn edit_storm_eastl() {
    if let Some(idx) = eastl() {
        storm("eastl", idx);
    }
}

#[test]
fn edit_storm_sdl_with_system_headers() {
    if let Some(idx) = sdl() {
        storm("sdl", idx);
    }
}

#[test]
fn edit_storm_msvc_stl() {
    if let Some(idx) = stl() {
        storm("msvc-stl", idx);
    }
}

/// Seed that once left a family empty: a definition written inside another
/// class's body shares its declaration's family but is a child of that class.
#[test]
fn edit_storm_regression_definition_inside_other_class() {
    if let Some(idx) = stl() {
        storm_with("msvc-stl regression", idx, 360, 7919, 1);
    }
}
