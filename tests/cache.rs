//! Disk-cache behaviour: a warm launch must reuse exactly what is still valid
//! and produce an index identical to a cold build, whatever changed in between.

use lightspeed::compile_commands::CompileDb;
use lightspeed::index::{CacheStats, Index, IndexConfig};
use lightspeed::pp::vfs::clear_dir_cache;
use lightspeed::store::CacheConfig;

/// The directory-listing cache is process-global, so launches must not interleave.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Ws {
    _serial: std::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    cache: tempfile::TempDir,
    root: String,
}

#[derive(Default, Clone)]
struct Flags {
    defines: Vec<&'static str>,
    inc: Vec<&'static str>,
    extra: Vec<&'static str>,
}

impl Ws {
    fn new(files: &[(&str, &str)]) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().replace('\\', "/");
        let ws = Self { _serial: serial, dir, cache: tempfile::tempdir().unwrap(), root };
        for (p, t) in files {
            ws.write(p, t);
        }
        ws
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.dir.path().join(rel)).unwrap();
    }

    fn cache_file(&self) -> std::path::PathBuf {
        CacheConfig::at(self.cache.path()).file()
    }

    /// One process lifetime: fresh directory cache, build (reading and then rewriting the cache).
    fn launch(&self, tus: &[&str], f: &Flags) -> Index {
        self.launch_with(tus, f, true)
    }

    fn launch_with(&self, tus: &[&str], f: &Flags, cache: bool) -> Index {
        clear_dir_cache();
        let entries: Vec<String> = tus
            .iter()
            .map(|t| {
                let mut args = vec!["clang++".to_owned(), "-c".into(), (*t).into()];
                for d in &f.defines {
                    args.push(format!("-D{d}"));
                }
                for i in &f.inc {
                    args.push("-I".into());
                    args.push(format!("{}/{i}", self.root));
                }
                args.extend(f.extra.iter().map(|s| (*s).to_owned()));
                let args: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
                format!(r#"{{"directory":"{}","file":"{t}","arguments":[{}]}}"#, self.root, args.join(","))
            })
            .collect();
        let cdb = CompileDb::from_json(&format!("[{}]", entries.join(","))).unwrap();
        let cfg = IndexConfig { cache: cache.then(|| CacheConfig::at(self.cache.path())), ..IndexConfig::default() };
        Index::build(cdb, cfg)
    }
}

fn cs(idx: &Index) -> &CacheStats {
    idx.stats.cache.as_ref().expect("cache stats")
}

/// Everything a query could observe, order-independent.
fn signature(idx: &Index) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for (i, f) in idx.live_files() {
        let syms: Vec<String> = idx.file_symbols(i).map(|s| format!("{:?} {}", idx.db.symbol(s).kind, idx.db.qualified_name(s))).collect();
        let rel = f.path.rsplit('/').next().unwrap_or(&f.path);
        v.push(format!("file {rel} h={:x} idents={} inc={} tu={} syms={syms:?}", f.content_hash, f.idents.len(), f.includes.len(), f.is_tu));
    }
    v.push(format!("families {} bases {}", idx.hier.stats.families, idx.hier.stats.bases_resolved));
    v.sort();
    v
}

fn base() -> Ws {
    Ws::new(&[
        ("inc/a.h", "#pragma once\nstruct A { void f(); };\n"),
        ("inc/b.h", "#pragma once\n#include \"a.h\"\nstruct B : A { void g(); };\n"),
        ("x.cpp", "#include \"b.h\"\nvoid xfn() { B b; b.g(); }\n"),
        ("y.cpp", "#include \"a.h\"\nvoid yfn() {}\n"),
        ("z.cpp", "struct Z { int v; };\n"),
    ])
}

const TUS: &[&str] = &["x.cpp", "y.cpp", "z.cpp"];

fn flags() -> Flags {
    Flags { inc: vec!["inc"], ..Flags::default() }
}

#[test]
fn warm_launch_reuses_everything_and_matches_cold() {
    let ws = base();
    let cold = ws.launch(TUS, &flags());
    assert_eq!(cs(&cold).hits, 0);
    assert_eq!(cs(&cold).absent, 5, "3 TUs + 2 headers");
    let sig = signature(&cold);
    drop(cold);
    let warm = ws.launch(TUS, &flags());
    assert_eq!((cs(&warm).hits, cs(&warm).dirty, cs(&warm).absent, cs(&warm).bad), (5, 0, 0, 0));
    assert_eq!(signature(&warm), sig);
    let nocache = ws.launch_with(TUS, &flags(), false);
    assert_eq!(signature(&nocache), sig);
}

#[test]
fn touching_without_changing_content_still_hits() {
    let ws = base();
    drop(ws.launch(TUS, &flags()));
    let text = std::fs::read_to_string(ws.dir.path().join("inc/a.h")).unwrap();
    ws.write("inc/a.h", &text);
    let warm = ws.launch(TUS, &flags());
    assert_eq!(cs(&warm).hits, 5);
}

#[test]
fn editing_a_tu_reanalyses_only_that_tu() {
    let ws = base();
    drop(ws.launch(TUS, &flags()));
    ws.write("z.cpp", "struct Z { int v; };\nstruct Z2 {};\n");
    let warm = ws.launch(TUS, &flags());
    assert_eq!((cs(&warm).hits, cs(&warm).dirty), (4, 1));
    assert!(signature(&warm).iter().any(|l| l.contains("Z2")));
    drop(warm);
    // The rewritten cache is itself consistent.
    assert_eq!(cs(&ws.launch(TUS, &flags())).hits, 5);
}

#[test]
fn editing_a_header_reanalyses_exactly_its_dependents() {
    let ws = base();
    drop(ws.launch(TUS, &flags()));
    ws.write("inc/a.h", "#pragma once\nstruct A { void f(); void added(); };\n");
    let warm = ws.launch(TUS, &flags());
    // a.h, b.h (includes a.h), x.cpp, y.cpp changed; z.cpp is independent.
    assert_eq!((cs(&warm).hits, cs(&warm).dirty), (1, 4));
    let cold = ws.launch_with(TUS, &flags(), false);
    assert_eq!(signature(&warm), signature(&cold));
    assert!(signature(&warm).iter().any(|l| l.contains("added")));
}

#[test]
fn new_header_shadowing_an_earlier_include_dir_invalidates_includers() {
    let ws = Ws::new(&[("inc2/dep.h", "struct Late {};\n"), ("x.cpp", "#include \"dep.h\"\n"), ("z.cpp", "struct Z {};\n")]);
    let f = Flags { inc: vec!["inc1", "inc2"], ..Flags::default() };
    let tus = &["x.cpp", "z.cpp"];
    drop(ws.launch(tus, &f));
    ws.write("inc1/dep.h", "struct Early {};\n");
    let warm = ws.launch(tus, &f);
    assert_eq!(cs(&warm).hits, 1, "only z.cpp: x.cpp now resolves to the new header, and inc2/dep.h is no longer included");
    let sig = signature(&warm);
    assert!(sig.iter().any(|l| l.contains("Early")) && !sig.iter().any(|l| l.contains("Late")));
    assert_eq!(sig, signature(&ws.launch_with(tus, &f, false)));
    assert!(cs(&warm).dirty >= 1);
}

#[test]
fn creating_a_missing_include_and_deleting_it_again() {
    let ws = Ws::new(&[
        ("x.cpp", "#include \"opt.h\"\n#if __has_include(\"opt2.h\")\nstruct HasOpt2 {};\n#endif\nstruct X {};\n"),
        ("z.cpp", "struct Z {};\n"),
    ]);
    let f = flags();
    let tus = &["x.cpp", "z.cpp"];
    let first = ws.launch(tus, &f);
    assert!(!signature(&first).iter().any(|l| l.contains("HasOpt2")));
    drop(first);

    ws.write("inc/opt.h", "struct Opt {};\n");
    ws.write("inc/opt2.h", "struct Opt2 {};\n");
    let created = ws.launch(tus, &f);
    assert_eq!(cs(&created).dirty, 1, "only x.cpp saw the missing files");
    let sig = signature(&created);
    assert!(sig.iter().any(|l| l.contains("HasOpt2")) && sig.iter().any(|l| l.contains("Opt\"")));
    assert_eq!(sig, signature(&ws.launch_with(tus, &f, false)));
    drop(created);

    ws.remove("inc/opt2.h");
    ws.remove("inc/opt.h");
    let deleted = ws.launch(tus, &f);
    assert!(cs(&deleted).dirty >= 1);
    let sig = signature(&deleted);
    assert!(!sig.iter().any(|l| l.contains("HasOpt2")));
    assert_eq!(sig, signature(&ws.launch_with(tus, &f, false)));
}

#[test]
fn flag_changes_never_reuse_records() {
    let ws = Ws::new(&[("inc/h.h", "#pragma once\n#ifdef FEAT\nstruct Feat {};\n#endif\n"), ("x.cpp", "#include \"h.h\"\n#if LEVEL > 1\nstruct Hi {};\n#endif\n")]);
    let tus = &["x.cpp"];
    let plain = flags();
    drop(ws.launch(tus, &plain));

    let feat = Flags { defines: vec!["FEAT"], ..plain.clone() };
    let a = ws.launch(tus, &feat);
    assert_eq!(cs(&a).hits, 0, "-D changes the effective flags");
    assert!(signature(&a).iter().any(|l| l.contains("Feat")));
    drop(a);

    let level = Flags { defines: vec!["FEAT", "LEVEL=2"], ..plain.clone() };
    let b = ws.launch(tus, &level);
    assert_eq!(cs(&b).hits, 0);
    assert!(signature(&b).iter().any(|l| l.contains("Hi")));
    drop(b);

    // Back to the original flags: still correct (the feature must be gone).
    let c = ws.launch(tus, &plain);
    let sig = signature(&c);
    assert!(!sig.iter().any(|l| l.contains("Feat") || l.contains("Hi")));
    assert_eq!(sig, signature(&ws.launch_with(tus, &plain, false)));
}

#[test]
fn include_dir_order_and_std_flag_changes_miss() {
    let ws = Ws::new(&[("inc1/h.h", "struct One {};\n"), ("inc2/h.h", "struct Two {};\n"), ("x.cpp", "#include \"h.h\"\n")]);
    let tus = &["x.cpp"];
    let a = Flags { inc: vec!["inc1", "inc2"], ..Flags::default() };
    let b = Flags { inc: vec!["inc2", "inc1"], ..Flags::default() };
    let sa = signature(&ws.launch(tus, &a));
    let wb = ws.launch(tus, &b);
    assert_eq!(cs(&wb).hits, 0);
    let sb = signature(&wb);
    assert_ne!(sa, sb);
    assert!(sb.iter().any(|l| l.contains("Two")));
    drop(wb);
    let std20 = Flags { extra: vec!["-std=c++20"], ..b.clone() };
    assert_eq!(cs(&ws.launch(tus, &std20)).hits, 0);
}

#[test]
fn force_include_changes_are_tracked() {
    let ws = Ws::new(&[("pre.h", "#define MODE 1\n"), ("x.cpp", "#if MODE\nstruct On {};\n#endif\n")]);
    let tus = &["x.cpp"];
    let pre = format!("{}/pre.h", ws.root);
    let pre: &'static str = Box::leak(pre.into_boxed_str());
    let f = Flags { extra: vec!["-include", pre], ..Flags::default() };
    let first = ws.launch(tus, &f);
    assert!(signature(&first).iter().any(|l| l.contains("On")));
    drop(first);
    ws.write("pre.h", "#define MODE 0\n");
    let second = ws.launch(tus, &f);
    assert_eq!(cs(&second).dirty, 1);
    assert!(!signature(&second).iter().any(|l| l.contains("On")));
}

#[test]
fn computed_include_follows_its_macro() {
    let ws = Ws::new(&[("inc/p1.h", "struct P1 {};\n"), ("inc/p2.h", "struct P2 {};\n"), ("x.cpp", "#define WHICH \"p1.h\"\n#include WHICH\n")]);
    let tus = &["x.cpp"];
    drop(ws.launch(tus, &flags()));
    ws.write("inc/p1.h", "struct P1 {};\nstruct P1b {};\n");
    let warm = ws.launch(tus, &flags());
    assert!(cs(&warm).dirty >= 1);
    assert!(signature(&warm).iter().any(|l| l.contains("P1b")));
}

#[test]
fn deleting_a_translation_unit_is_reported_and_survives_relaunch() {
    let ws = base();
    drop(ws.launch(TUS, &flags()));
    ws.remove("z.cpp");
    let warm = ws.launch(TUS, &flags());
    assert_eq!(warm.unreadable, 1);
    assert!(!signature(&warm).iter().any(|l| l.starts_with("file z.cpp")));
    drop(warm);
    let again = ws.launch(TUS, &flags());
    assert_eq!(again.unreadable, 1);
    assert_eq!(signature(&again), signature(&ws.launch_with(TUS, &flags(), false)));
}

#[test]
fn different_executable_fingerprint_discards_the_cache() {
    let ws = base();
    let sig = signature(&ws.launch(TUS, &flags()));
    let path = ws.cache_file();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[8] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    let warm = ws.launch(TUS, &flags());
    assert_eq!(cs(&warm).status, "Stale");
    assert_eq!(cs(&warm).hits, 0);
    assert_eq!(signature(&warm), sig);
    assert_eq!(cs(&ws.launch(TUS, &flags())).hits, 5, "the rebuilt cache is valid again");
}

#[test]
fn damaged_cache_files_fall_back_to_a_correct_build() {
    let ws = base();
    let sig = signature(&ws.launch(TUS, &flags()));
    let path = ws.cache_file();
    let good = std::fs::read(&path).unwrap();

    let mut cases: Vec<(String, Vec<u8>)> = vec![("empty".into(), vec![]), ("garbage".into(), vec![0xAB; 4096]), ("magic only".into(), good[..4].to_vec())];
    for cut in [good.len() - 1, good.len() - 64, good.len() / 2, 40] {
        cases.push((format!("truncated to {cut}"), good[..cut].to_vec()));
    }
    let step = (good.len() / 97).max(1);
    for at in (0..good.len()).step_by(step) {
        let mut b = good.clone();
        b[at] ^= 0x5A;
        cases.push((format!("flip byte {at}"), b));
    }
    for (name, bytes) in cases {
        std::fs::write(&path, &bytes).unwrap();
        let idx = ws.launch(TUS, &flags());
        assert_eq!(signature(&idx), sig, "{name}");
        drop(idx);
        // Whatever happened, the next launch must see a healthy cache.
        let again = ws.launch(TUS, &flags());
        assert_eq!(signature(&again), sig, "{name} (relaunch)");
        assert_eq!(cs(&again).hits, 5, "{name} (relaunch)");
    }
    std::fs::remove_file(&path).unwrap();
    assert_eq!(cs(&ws.launch(TUS, &flags())).status, "Missing");
}

#[test]
fn successive_partial_invalidations_stay_consistent() {
    let ws = base();
    drop(ws.launch(TUS, &flags()));
    for round in 0..4 {
        match round {
            0 => ws.write("inc/a.h", "#pragma once\nstruct A { void f(); int r0; };\n"),
            1 => ws.write("z.cpp", "struct Z { int v; int r1; };\n"),
            2 => ws.write("y.cpp", "#include \"a.h\"\nvoid yfn() {}\nstruct Y2 {};\n"),
            _ => ws.write("inc/b.h", "#pragma once\n#include \"a.h\"\nstruct B : A { void g(); void r3(); };\n"),
        }
        let warm = ws.launch(TUS, &flags());
        let cold = ws.launch_with(TUS, &flags(), false);
        assert_eq!(signature(&warm), signature(&cold), "round {round}");
        drop(warm);
        let next = ws.launch(TUS, &flags());
        assert_eq!(cs(&next).hits, 5, "round {round}: cache rewritten with fresh and reused records");
    }
}

#[test]
fn header_edited_after_save_is_caught_by_content_not_mtime() {
    let ws = base();
    drop(ws.launch(TUS, &flags()));
    let p = ws.dir.path().join("inc/a.h");
    let before = std::fs::metadata(&p).unwrap().modified().unwrap();
    // Same length, different content, mtime restored.
    ws.write("inc/a.h", "#pragma once\nstruct Q { void f(); };\n");
    let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    f.set_modified(before).unwrap();
    drop(f);
    let warm = ws.launch(TUS, &flags());
    assert!(cs(&warm).dirty >= 3);
    assert!(signature(&warm).iter().any(|l| l.contains("Q")));
}
