//! `cdb` (compile database stats) and `pp` / `parse` (preprocess / parse every translation unit).

use std::time::Instant;

use lightspeed::SourceFile;
use rayon::prelude::*;

pub(super) fn cdb_cmd(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: lightspeed cdb <compile_commands.json>");
        return 2;
    };
    let t = Instant::now();
    let db = match super::load_cdb(path) {
        Some(db) => db,
        None => return 1,
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

pub(super) fn pp_cmd(threads: usize, args: &[String]) -> i32 {
    pp_or_parse(threads, args, false)
}

pub(super) fn parse_cmd(threads: usize, args: &[String]) -> i32 {
    pp_or_parse(threads, args, true)
}

pub(super) fn pp_or_parse(threads: usize, args: &[String], parse: bool) -> i32 {
    use lightspeed::pp::vfs::DiskFs;
    use lightspeed::pp::{PpConfig, PpOutput, Preprocessor};
    let Some(path) = args.first() else {
        eprintln!("usage: lightspeed pp|parse [-j threads] <compile_commands.json> [--no-follow]");
        return 2;
    };
    let follow = !args.iter().any(|a| a == "--no-follow");
    let db = match super::load_cdb(path) {
        Some(db) => db,
        None => return 1,
    };
    let pool = super::pool(threads);
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
