use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use lightspeed::testgen::cpp_source;
use lightspeed::{Lexer, LoadOptions, SourceFile, Token, lex_into};

fn lexing(c: &mut Criterion) {
    let mut g = c.benchmark_group("lex");
    for &size in &[16 * 1024, 256 * 1024, 4 * 1024 * 1024] {
        let src = cpp_source(size, 42);
        g.throughput(Throughput::Bytes(src.len() as u64));
        g.bench_with_input(BenchmarkId::new("iterate", size), &src, |b, s| {
            b.iter(|| {
                let mut n = 0u32;
                for t in Lexer::new(black_box(s)) {
                    n = n.wrapping_add(t.len);
                }
                n
            })
        });
        let mut buf: Vec<Token> = Vec::new();
        g.bench_with_input(BenchmarkId::new("lex_into_reused", size), &src, |b, s| {
            b.iter(|| {
                lex_into(black_box(s), &mut buf);
                buf.len()
            })
        });
        g.bench_with_input(BenchmarkId::new("with_comments", size), &src, |b, s| {
            b.iter(|| Lexer::new(black_box(s)).emit_comments(true).count())
        });
    }
    g.finish();
}

fn loading(c: &mut Criterion) {
    let mut g = c.benchmark_group("load");
    let dir = tempfile::tempdir().unwrap();
    for &size in &[4 * 1024, 64 * 1024, 1024 * 1024] {
        let path = dir.path().join(format!("f{size}.h"));
        let text = cpp_source(size, 9);
        std::fs::write(&path, &text).unwrap();
        g.throughput(Throughput::Bytes(text.len() as u64));
        for (name, threshold) in [("mmap", 0), ("read", u64::MAX)] {
            let opts = LoadOptions { mmap_threshold: threshold, lossy_utf8: true };
            g.bench_with_input(BenchmarkId::new(name, size), &path, |b, p| {
                b.iter_batched(|| (), |_| SourceFile::load_with(p, &opts).unwrap().len(), BatchSize::SmallInput)
            });
            g.bench_with_input(BenchmarkId::new(format!("{name}_and_lex"), size), &path, |b, p| {
                b.iter(|| {
                    let f = SourceFile::load_with(p, &opts).unwrap();
                    Lexer::new(f.text()).count()
                })
            });
        }
    }
    g.finish();
}

criterion_group!(benches, lexing, loading);
criterion_main!(benches);
