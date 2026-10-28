use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use lightspeed::workspace::compile_commands::FlagSet;
use lightspeed::parser::{ParseOutput, parse};
use lightspeed::pp::vfs::MemFs;
use lightspeed::pp::{PpConfig, PpOutput, Preprocessor};
use lightspeed::symbols::SymbolDb;
use lightspeed::testgen::cpp_source;
use std::hint::black_box;

fn parser(c: &mut Criterion) {
    let src = cpp_source(256 * 1024, 7);
    let fs = MemFs::new();
    let mut pp = Preprocessor::new(&fs, PpConfig::default());
    pp.set_flags(&FlagSet::default());
    let mut out = PpOutput::default();
    pp.run("/b/main.cpp", &src, &mut out);

    let mut g = c.benchmark_group("parser");
    g.throughput(Throughput::Bytes(src.len() as u64));
    let mut res = ParseOutput::default();
    g.bench_function("parse_256k", |b| {
        b.iter(|| {
            let mut db = SymbolDb::new();
            let file = db.files.intern("/b/main.cpp");
            parse(&out, &src, &mut db, file, &mut res);
            black_box(db.len())
        })
    });
    g.bench_function("pp_plus_parse_256k", |b| {
        b.iter(|| {
            pp.run("/b/main.cpp", &src, &mut out);
            let mut db = SymbolDb::new();
            let file = db.files.intern("/b/main.cpp");
            parse(&out, &src, &mut db, file, &mut res);
            black_box(db.len())
        })
    });
    g.finish();
}

criterion_group!(benches, parser);
criterion_main!(benches);
