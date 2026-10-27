use lightspeed::{SourceFile, Token, lex_into};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

// The system heap takes a lock on most allocations; mimalloc is per-thread.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const EXTS: &[&str] = &["h", "hpp", "hh", "hxx", "inl", "ipp", "c", "cc", "cpp", "cxx"];

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for e in rd.flatten() {
                collect(&e.path(), out);
            }
        }
    } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTS.contains(&e.to_ascii_lowercase().as_str())) {
        out.push(path.to_path_buf());
    }
}

fn lex_cmd(threads: usize, args: &[String]) -> i32 {
    let mut files = Vec::new();
    let t_walk = Instant::now();
    for a in args {
        collect(Path::new(a), &mut files);
    }
    let walk = t_walk.elapsed();

    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build().expect("thread pool");
    let t0 = Instant::now();
    // map_init gives each worker a private token buffer; collect keeps input order.
    let results: Vec<_> = pool.install(|| {
        files
            .par_iter()
            .map_init(Vec::<Token>::new, |buf, f| match SourceFile::load(f) {
                Ok(src) => {
                    lex_into(src.text(), buf);
                    Ok((src.len() as u64, buf.len() as u64))
                }
                Err(e) => Err(format!("{}: {e}", f.display())),
            })
            .collect()
    });
    let secs = t0.elapsed().as_secs_f64();

    let (mut bytes, mut tokens, mut failed) = (0u64, 0u64, 0u32);
    for r in results {
        match r {
            Ok((b, t)) => {
                bytes += b;
                tokens += t;
            }
            Err(e) => {
                eprintln!("{e}");
                failed += 1;
            }
        }
    }
    println!(
        "{} files ({failed} failed), {:.1} MiB, {tokens} tokens | walk {:.1} ms | lex on {threads} threads {:.1} ms ({:.0} MiB/s)",
        files.len(),
        bytes as f64 / 1048576.0,
        walk.as_secs_f64() * 1e3,
        secs * 1e3,
        bytes as f64 / 1048576.0 / secs.max(1e-9)
    );
    i32::from(failed > 0)
}

fn gen_cmd(args: &[String]) -> i32 {
    let (Some(dir), Some(count), Some(size)) = (
        args.first(),
        args.get(1).and_then(|s| s.parse::<usize>().ok()),
        args.get(2).and_then(|s| s.parse::<usize>().ok()),
    ) else {
        eprintln!("usage: lightspeed gen <dir> <files> <bytes-per-file>");
        return 2;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("{e}");
        return 1;
    }
    let errs: Vec<bool> = (0..count)
        .into_par_iter()
        .map(|i| {
            let text = lightspeed::testgen::cpp_source(size, i as u64);
            std::fs::write(Path::new(dir).join(format!("gen{i}.h")), text).is_err()
        })
        .collect();
    println!("wrote {} files to {dir}", count - errs.iter().filter(|&&e| e).count());
    i32::from(errs.iter().any(|&e| e))
}

fn cdb_cmd(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: lightspeed cdb <compile_commands.json>");
        return 2;
    };
    let t = Instant::now();
    let db = match lightspeed::compile_commands::CompileDb::load(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let (mut dirs, mut macros) = (0, 0);
    for fs in db.flag_sets.iter() {
        dirs += fs.include_dirs.len();
        macros += fs.macros.len();
    }
    println!(
        "{} entries, {} unique flag sets ({dirs} include dirs, {macros} macro ops total) in {:.1} ms",
        db.entries.len(),
        db.flag_sets.len(),
        t.elapsed().as_secs_f64() * 1e3
    );
    0
}

fn pp_cmd(threads: usize, args: &[String]) -> i32 {
    pp_or_parse(threads, args, false)
}

fn parse_cmd(threads: usize, args: &[String]) -> i32 {
    pp_or_parse(threads, args, true)
}

fn pp_or_parse(threads: usize, args: &[String], parse: bool) -> i32 {
    use lightspeed::pp::vfs::DiskFs;
    use lightspeed::pp::{PpConfig, PpOutput, Preprocessor};
    let Some(path) = args.first() else {
        eprintln!("usage: lightspeed pp|parse [-j threads] <compile_commands.json> [--no-follow]");
        return 2;
    };
    let follow = !args.iter().any(|a| a == "--no-follow");
    let db = match lightspeed::compile_commands::CompileDb::load(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build().expect("thread pool");
    let fs = DiskFs;
    let t0 = Instant::now();
    // Each worker owns its Preprocessor and output buffers; nothing is shared but the read-only fs.
    let results: Vec<(u64, u64, u64, u32, u32, u64)> = pool.install(|| {
        db.entries
            .par_iter()
            .map_init(
                || (Preprocessor::new(&fs, PpConfig { follow_includes: follow, ..PpConfig::default() }), PpOutput::default(), u64::MAX, lightspeed::parser::ParseOutput::default()),
                |(pp, out, last, pres), e| {
                    if *last != e.flags_hash {
                        pp.set_flags(db.flags(e));
                        *last = e.flags_hash;
                    }
                    let Ok(src) = SourceFile::load(&e.file) else { return (0, 0, 0, 1, 0, 0) };
                    pp.run(&e.file, src.text(), out);
                    let mut syms = 0;
                    if parse {
                        let mut shard = lightspeed::symbols::SymbolDb::new();
                        let file = shard.files.intern(&e.file);
                        lightspeed::parser::parse(out, src.text(), &mut shard, file, pres);
                        syms = shard.len() as u64;
                    }
                    (src.len() as u64, out.tokens.len() as u64, out.stats.headers_entered as u64, 0, out.diagnostics.len() as u32, syms)
                },
            )
            .collect()
    });
    let secs = t0.elapsed().as_secs_f64();
    let (mut bytes, mut toks, mut hdrs, mut failed, mut diags, mut syms) = (0, 0, 0, 0, 0u64, 0u64);
    for (b, t, h, f, d, s) in results {
        bytes += b;
        toks += t;
        hdrs += h;
        failed += f;
        diags += d as u64;
        syms += s;
    }
    println!(
        "{} TUs ({failed} unreadable), {:.1} MiB, {toks} out tokens, {hdrs} headers scanned, {diags} diags, {syms} symbols | {} on {threads} threads {:.1} ms",
        db.entries.len(),
        bytes as f64 / 1048576.0,
        if parse { "pp+parse" } else { "pp" },
        secs * 1e3
    );
    0
}

fn index_cmd(threads: usize, args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: lightspeed index [-j threads] <compile_commands.json>");
        return 2;
    };
    let mut cdb = match lightspeed::compile_commands::CompileDb::load(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if !args.iter().any(|a| a == "--no-sysinc") {
        println!("{} built-in include dirs", cdb.add_system_includes());
    }
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build().expect("thread pool");
    let mut cfg = lightspeed::index::IndexConfig::default();
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
    let idx = pool.install(|| lightspeed::index::Index::build(cdb, cfg));
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
    0
}

fn def_cmd(threads: usize, args: &[String]) -> i32 {
    use lightspeed::query::{DiskText, Query};
    let [cdb_path, file, line, col] = args else {
        eprintln!("usage: lightspeed def <compile_commands.json> <file> <line> <col>   (1-based)");
        return 2;
    };
    let Ok(mut cdb) = lightspeed::compile_commands::CompileDb::load(cdb_path) else { return 1 };
    cdb.add_system_includes();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build().expect("thread pool");
    let idx = pool.install(|| lightspeed::index::Index::build(cdb, Default::default()));
    let text = std::fs::read_to_string(file).expect("read file");
    let li = lightspeed::source::LineIndex::new(&text);
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

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    if let Some(i) = args.iter().position(|a| a == "-j") {
        match args.get(i + 1).and_then(|n| n.parse().ok()) {
            Some(n) => threads = n,
            None => {
                eprintln!("-j needs a number");
                std::process::exit(2);
            }
        }
        args.drain(i..=i + 1);
    }
    let code = match args.split_first() {
        Some((cmd, rest)) if cmd == "lex" && !rest.is_empty() => lex_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "gen" => gen_cmd(rest),
        Some((cmd, rest)) if cmd == "cdb" => cdb_cmd(rest),
        Some((cmd, rest)) if cmd == "pp" => pp_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "parse" => parse_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "index" => index_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "def" => def_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "lsp" => {
            let cdb = rest.iter().position(|a| a == "--cdb").and_then(|i| rest.get(i + 1)).map(std::path::PathBuf::from);
            if rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build_global().is_err() {
                eprintln!("lightspeed: thread pool already initialised");
            }
            lightspeed::lsp::serve_stdio(cdb)
        }
        _ => {
            eprintln!("usage: lightspeed lex [-j threads] <file|dir>... | gen <dir> <files> <bytes> | cdb <compile_commands.json> | pp|parse|index [-j threads] <compile_commands.json> | lsp [--cdb compile_commands.json]");
            2
        }
    };
    std::process::exit(code);
}
