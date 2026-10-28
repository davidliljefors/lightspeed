//! `index` (full build with timing and memory report) and `def` (one go-to-definition from the command line).

use std::time::Instant;

pub(super) fn index_cmd(threads: usize, args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: lightspeed index [-j threads] <compile_commands.json>");
        return 2;
    };
    let mut cdb = match super::load_cdb(path) {
        Some(db) => db,
        None => return 1,
    };
    if !args.iter().any(|a| a == "--no-sysinc") {
        println!("{} built-in include dirs", cdb.add_system_includes());
    }
    let pool = super::pool(threads);
    let mut cfg = lightspeed::workspace::index::IndexConfig::default();
    if !args.iter().any(|a| a == "--no-cache") {
        cfg.cache = match args.iter().position(|a| a == "--cache-dir").and_then(|i| args.get(i + 1)) {
            Some(d) => Some(lightspeed::store::CacheConfig::at(d)),
            None => lightspeed::store::CacheConfig::for_project(std::path::Path::new(path)),
        };
        if let Some(c) = cfg.cache.as_mut() {
            c.background_save = false;
        }
    }
    let t0 = Instant::now();
    let idx = pool.install(|| lightspeed::workspace::index::Index::build(cdb, cfg));
    println!(
        "{} files ({} unreadable), {} symbols, {} families, {} bases resolved | build on {threads} threads {:.1} ms",
        idx.live_files().count(),
        idx.unreadable,
        idx.db.len(),
        idx.hier.stats.families,
        idx.hier.stats.bases_resolved,
        t0.elapsed().as_secs_f64() * 1e3
    );
    println!("{}", idx.stats.report(threads));
    println!("{}", idx.memory_report());
    if args.iter().any(|a| a == "--occurrences") {
        use lightspeed::base::mem::{mib, process_bytes};
        use lightspeed::query::{DiskText, Query};
        let before = process_bytes().0;
        let t1 = Instant::now();
        let (occ, st) = pool.install(|| Query::new(&idx, &DiskText).build_occurrences());
        let secs = t1.elapsed().as_secs_f64();
        let after = process_bytes();
        println!(
            "occurrences: {} files, {} candidate identifiers ({} member accesses, {} locals skipped) in {:.2} s on {threads} threads",
            st.files, st.candidates, st.members, st.locals, secs
        );
        println!(
            "  stored {} certain ({:.1}%), {} ambiguous, {} external, {} unresolved | table {} | resident {} -> {} (peak {})",
            st.stored,
            100.0 * st.stored as f64 / st.candidates.max(1) as f64,
            st.ambiguous,
            st.external,
            st.unresolved,
            mib(st.bytes),
            mib(before),
            mib(after.0),
            mib(after.1)
        );
        if let Some(p) = args.iter().position(|a| a == "--sample-out").and_then(|i| args.get(i + 1)) {
            let q = Query::new(&idx, &DiskText);
            let lc = |path: &str, off: u32| -> (usize, usize) {
                let t = std::fs::read_to_string(path).unwrap_or_default();
                let head = &t[..(off as usize).min(t.len())];
                (head.matches('\n').count(), head.len() - head.rfind('\n').map_or(0, |n| n + 1))
            };
            let mut out = String::new();
            let stride: usize = args.iter().position(|a| a == "--sample-stride").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(25);
            for (fi, f) in idx.live_files() {
                if fi % stride != 0 || occ[fi].sites.is_empty() {
                    continue;
                }
                let step = (occ[fi].sites.len() / 20).max(1);
                for &(off, sym) in occ[fi].sites.iter().step_by(step) {
                    let Some(loc) = q.location(sym) else { continue };
                    let dpath = idx.files[loc.file as usize].path.as_str();
                    let ((l, c), (dl, dc)) = (lc(&f.path, off), lc(dpath, loc.start));
                    let recs: Vec<String> = q
                        .entity_records(sym)
                        .into_iter()
                        .filter_map(|r| q.location(r))
                        .map(|rl| {
                            let p = idx.files[rl.file as usize].path.as_str();
                            format!("{p}:{}", lc(p, rl.start).0)
                        })
                        .collect();
                    out.push_str(&format!("{}\t{l}\t{c}\t{}\t{dpath}\t{dl}\t{dc}\t{}\n", f.path, idx.db.qualified_name(sym), recs.join(";")));
                }
            }
            std::fs::write(p, out).unwrap();
        }
        drop(occ);
    }
    0
}

pub(super) fn def_cmd(threads: usize, args: &[String]) -> i32 {
    use lightspeed::query::{DiskText, Query};
    let [cdb_path, file, line, col] = args else {
        eprintln!("usage: lightspeed def <compile_commands.json> <file> <line> <col>   (1-based)");
        return 2;
    };
    let Ok(mut cdb) = lightspeed::workspace::compile_commands::CompileDb::load(cdb_path) else { return 1 };
    cdb.add_system_includes();
    let pool = super::pool(threads);
    let idx = pool.install(|| lightspeed::workspace::index::Index::build(cdb, Default::default()));
    let text = std::fs::read_to_string(file).expect("read file");
    let li = lightspeed::base::source::LineIndex::new(&text);
    let off = li.line_start(line.parse::<usize>().unwrap() - 1).unwrap() + col.parse::<u32>().unwrap() - 1;
    let mut path = file.replace('\\', "/");
    lightspeed::pp::vfs::normalize_path(&mut path);
    let q = Query::new(&idx, &DiskText);
    let defs = q.definition(&path, &text, off);
    for d in &defs {
        let s = idx.db.symbol(*d);
        println!("{} {:?} {}:{}", idx.db.qualified_name(*d), s.kind, idx.db.file_path(*d), s.name_offset);
    }
    println!("{} result(s)", defs.len());
    0
}
