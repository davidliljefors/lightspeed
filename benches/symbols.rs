use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use lightspeed::base::csr::Csr;
use lightspeed::base::interner::Interner;
use lightspeed::symbols::{Symbol, SymbolDb, SymbolKind};
use lightspeed::{NameId, SymbolId};

fn names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("Symbol{}_{}", i % 5000, i / 5000)).collect()
}

fn build_db(n: usize) -> SymbolDb {
    let strings = names(n);
    let mut db = SymbolDb::new();
    let file = db.files.intern("src/engine/actor.h");
    let mut last_scope = None;
    for (i, s) in strings.iter().enumerate() {
        let name = db.intern(s);
        let kind = if i % 20 == 0 { SymbolKind::Class } else { SymbolKind::Method };
        let parent = if kind == SymbolKind::Class { None } else { last_scope };
        let id = db.add_symbol(Symbol::new(name, kind, file, i as u32 * 16).with_parent(parent));
        if kind == SymbolKind::Class {
            last_scope = Some(id);
        }
    }
    db
}

fn interning(c: &mut Criterion) {
    let mut g = c.benchmark_group("interner");
    let strings = names(100_000);
    g.throughput(Throughput::Elements(strings.len() as u64));
    g.bench_function("intern_100k_mostly_unique", |b| {
        b.iter(|| {
            let mut i: Interner<NameId> = Interner::new();
            for s in &strings {
                i.intern(s);
            }
            i.len()
        })
    });
    let mut warm: Interner<NameId> = Interner::new();
    for s in &strings {
        warm.intern(s);
    }
    g.bench_function("lookup_hit_100k", |b| {
        b.iter(|| strings.iter().filter(|s| warm.lookup(black_box(s)).is_some()).count())
    });
    g.finish();
}

fn database(c: &mut Criterion) {
    let mut g = c.benchmark_group("symbol_db");
    for &n in &[10_000usize, 200_000, 1_000_000] {
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("insert", n), &n, |b, &n| b.iter(|| build_db(n).len()));
        let mut db = build_db(n);
        g.bench_with_input(BenchmarkId::new("rebuild_indexes", n), &n, |b, _| {
            b.iter(|| {
                db.rebuild_indexes();
                db.len()
            })
        });
        db.rebuild_indexes();
        g.bench_with_input(BenchmarkId::new("lookup_by_name", n), &n, |b, _| {
            b.iter(|| db.lookup(black_box("Symbol42_1")).len())
        });
        let scope = db.symbols().ids().find(|&id| db.symbol(id).kind == SymbolKind::Class).unwrap();
        g.bench_with_input(BenchmarkId::new("children_of_scope", n), &n, |b, _| {
            b.iter(|| db.children(black_box(scope)).len())
        });
        let leaf = db.symbols().ids().last().unwrap();
        let mut buf = String::with_capacity(256);
        g.bench_with_input(BenchmarkId::new("qualified_name", n), &n, |b, _| {
            b.iter(|| {
                buf.clear();
                db.write_qualified_name(black_box(leaf), &mut buf);
                buf.len()
            })
        });
    }
    g.finish();
}

fn csr(c: &mut Criterion) {
    let mut g = c.benchmark_group("csr");
    let n = 1_000_000usize;
    let pairs: Vec<(SymbolId, u32)> = {
        let db = build_db(1000);
        let ids: Vec<SymbolId> = db.symbols().ids().collect();
        (0..n).map(|i| (ids[(i * 7919) % ids.len()], i as u32)).collect()
    };
    g.throughput(Throughput::Elements(n as u64));
    g.bench_function("build_1m_pairs_1k_keys", |b| {
        b.iter(|| Csr::build(1000, pairs.iter().copied()).len())
    });
    g.finish();
}

criterion_group!(benches, interning, database, csr);
criterion_main!(benches);
