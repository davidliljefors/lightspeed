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
    print!("{}", lightspeed::pp::profile_report());
    if let Some(out) = args.iter().position(|a| a == "--dump").and_then(|i| args.get(i + 1)) {
        let mut lines: Vec<String> = idx
            .db
            .symbols()
            .iter_enumerated()
            .filter(|(id, _)| !idx.db.is_dead(*id))
            .map(|(id, s)| format!("{}|{:?}|{}|{}|{}-{}|{:?}|{}", idx.db.qualified_name(id), s.kind, idx.db.file_path(id), s.name_offset, s.extent_start, s.extent_end, s.access, s.flags.bits()))
            .collect();
        lines.sort_unstable();
        lines.push(format!("families={} bases={}", idx.hier.stats.families, idx.hier.stats.bases_resolved));
        if let Err(e) = std::fs::write(out, lines.join("\n")) {
            eprintln!("dump: {e}");
            return 1;
        }
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
