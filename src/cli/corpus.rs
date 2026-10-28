//! `lex` (tokenizer throughput over a tree) and `gen` (synthetic corpus generator).

use std::path::{Path, PathBuf};
use std::time::Instant;

use lightspeed::{SourceFile, Token, lex_into};
use rayon::prelude::*;

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

pub(super) fn lex_cmd(threads: usize, args: &[String]) -> i32 {
    let mut files = Vec::new();
    let t_walk = Instant::now();
    for a in args {
        collect(Path::new(a), &mut files);
    }
    let walk = t_walk.elapsed();

    let pool = super::pool(threads);
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

pub(super) fn gen_cmd(args: &[String]) -> i32 {
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
