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
        if let Some(names) = args.iter().position(|a| a == "--refs-of").and_then(|i| args.get(i + 1)) {
          for name in names.split(',') {
            let q = Query::new(&idx, &DiskText);
            let last = name.rsplit("::").next().unwrap_or(name);
            for &seed in idx.db.lookup(last).iter().filter(|&&c| idx.db.qualified_name(c) == name && q.location(c).is_some()).take(1) {
                let loc = q.location(seed).unwrap();
                let (path, off) = (idx.files[loc.file as usize].path.as_str(), loc.start);
                let text = std::fs::read_to_string(path).unwrap();
                let t = Instant::now();
                let r = q.symbol_refs(path, &text, off).unwrap();
                let inference = t.elapsed();
                let t = Instant::now();
                let targets: std::collections::HashSet<_> = q.entity_records(seed).into_iter().chain(r.targets.iter().copied()).collect();
                let mut hits: Vec<(u32, u32)> = Vec::new();
                for (fi, o) in occ.iter().enumerate() {
                    hits.extend(o.sites.iter().filter(|(_, s)| targets.contains(s)).map(|&(off, _)| (fi as u32, off)));
                }
                let lookup = t.elapsed();
                let certain: std::collections::HashSet<_> = r.certain.iter().map(|l| (l.file, l.start)).collect();
                let stored: std::collections::HashSet<_> = hits.iter().copied().collect();
                let only_occ: Vec<_> = stored.difference(&certain).collect();
                println!(
                    "refs-of {name}: inference certain {} + uncertain {} in {:.1} ms | occurrence lookup {} in {:.1} ms | only in table {}, only in inference-certain {}",
                    r.certain.len(), r.uncertain.len(), inference.as_secs_f64() * 1e3, hits.len(), lookup.as_secs_f64() * 1e3, only_occ.len(), certain.difference(&stored).count()
                );
                for &&(f, o) in only_occ.iter().take(5) {
                    println!("  only table: {}:{o}", idx.files[f as usize].path);
                }
                for &(f, o) in certain.difference(&stored).take(5) {
                    println!("  only inference: {}:{o}", idx.files[f as usize].path);
                }
            }
          }
        }
        drop(occ);
    }
    0
}

pub(super) fn refs_cmd(threads: usize, args: &[String]) -> i32 {
    use lightspeed::query::Query;
    use std::sync::atomic::Ordering::Relaxed;
    let [cdb_path, file, line, col] = args else {
        eprintln!("usage: lightspeed refs <compile_commands.json> <file> <line> <col>   (1-based)");
        return 2;
    };
    let Ok(mut cdb) = lightspeed::workspace::compile_commands::CompileDb::load(cdb_path) else { return 1 };
    cdb.add_system_includes();
    let pool = super::pool(threads);
    let cfg = lightspeed::workspace::index::IndexConfig {
        cache: lightspeed::store::CacheConfig::for_project(std::path::Path::new(cdb_path)),
        ..Default::default()
    };
    let t = Instant::now();
    let idx = pool.install(|| lightspeed::workspace::index::Index::build(cdb, cfg));
    println!("index {:.1} s", t.elapsed().as_secs_f64());
    let text = std::fs::read_to_string(file).expect("read file");
    let li = lightspeed::base::source::LineIndex::new(&text);
    let off = li.line_start(line.parse::<usize>().unwrap() - 1).unwrap() + col.parse::<u32>().unwrap() - 1;
    let mut path = file.replace('\\', "/");
    lightspeed::pp::vfs::normalize_path(&mut path);
    let q = Query::new(&idx, &*idx.texts);
    if std::env::var_os("LS_NOWARM").is_none() {
        let paths: Vec<String> = idx.live_files().map(|(_, f)| f.path.clone()).collect();
        let t = Instant::now();
        pool.install(|| idx.texts.warm(&paths));
        let (n, raw, packed) = idx.texts.stats();
        println!("text store: {n} files, {} MB raw as {} MB lz4, warmed in {:.1} s", raw >> 20, packed >> 20, t.elapsed().as_secs_f64());
    }
    if let Ok(needle) = std::env::var("LS_GREP") {
        use rayon::prelude::*;
        let paths: Vec<&str> = idx.live_files().map(|(_, f)| f.path.as_str()).collect();
        for run in 0..2 {
            let t = Instant::now();
            let hits = pool.install(|| {
                paths.par_iter().filter(|p| idx.texts.get(p).is_some_and(|t| memchr::memmem::find(t.as_bytes(), needle.as_bytes()).is_some())).count()
            });
            println!("grep {run}: {hits} files with hits in {:.0} ms over {} files", t.elapsed().as_secs_f64() * 1e3, paths.len());
        }
        return 0;
    }
    for run in 0..2 {
        for p in lightspeed::query::refs::PROF.iter() {
            p.store(0, Relaxed);
        }
        let t = Instant::now();
        let r = pool.install(|| q.symbol_refs(&path, &text, off));
        let wall = t.elapsed().as_secs_f64() * 1e3;
        let p = |i: usize| lightspeed::query::refs::PROF[i].load(Relaxed);
        let (c, u) = r.map_or((0, 0), |r| (r.certain.len(), r.uncertain.len()));
        println!(
            "run {run}: wall {wall:.0} ms | certain {c} uncertain {u} | candidates {} read {} files w/ name {} | {:.0} MB | thread-time: read {:.0} ms, lex {:.0} ms, resolve {:.0} ms",
            p(6), p(4), p(5), p(3) as f64 / 1e6, p(0) as f64 / 1e6, p(1) as f64 / 1e6, p(2) as f64 / 1e6
        );
        println!(
            "   definition_in calls {} (member {}), avg cands {:.0} | thread-time: local_in {:.0} ms, scopes {:.0} ms, lookup {:.0} ms, receiver {:.0} ms",
            p(7), p(11), p(12) as f64 / p(7).max(1) as f64, p(13) as f64 / 1e6, p(8) as f64 / 1e6, p(9) as f64 / 1e6, p(10) as f64 / 1e6
        );
        let ms = |i: usize| p(i) as f64 / 1e6;
        let n = p(7).max(1) as f64;
        println!(
            "   stages ms: sort/dedup {:.0}, qualifier {:.0}, drop_fwd {:.0}, spec {:.0}, unqual {:.0}, score {:.0}, overloads {:.0} | avg cands after: dedup {:.0}, drop_fwd {:.0}, pre-score {:.0}, picked {:.1} | qualified calls {}, unqualified {}",
            ms(14), ms(15), ms(16), ms(17), ms(18), ms(19), ms(20),
            p(24) as f64 / n, p(26) as f64 / n, p(28) as f64 / n, p(29) as f64 / n, p(25), p(27)
        );
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
