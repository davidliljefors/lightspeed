use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use lightspeed::store::codec::{ShardKey, decode_shard, encode_shard};
use lightspeed::workspace::compile_commands::CompileDb;
use lightspeed::base::hash::hash_bytes;
use lightspeed::symbols::{Symbol, SymbolDb, SymbolKind};
use lightspeed::testgen::{compile_commands_json, cpp_source};
use std::hint::black_box;

fn hashing(c: &mut Criterion) {
    let mut g = c.benchmark_group("hash");
    for &size in &[4 * 1024, 256 * 1024, 8 * 1024 * 1024] {
        let data = cpp_source(size, 1);
        g.throughput(Throughput::Bytes(data.len() as u64));
        g.bench_with_input(BenchmarkId::new("xxh3", size), &data, |b, d| b.iter(|| hash_bytes(black_box(d.as_bytes()))));
    }
    g.finish();
}

fn compile_commands(c: &mut Criterion) {
    let mut g = c.benchmark_group("compile_commands");
    for &n in &[1_000usize, 50_000] {
        let json = compile_commands_json(n, 20);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("from_json", n), &json, |b, j| {
            b.iter(|| CompileDb::from_json(black_box(j)).unwrap().entries.len())
        });
    }
    g.finish();
}

fn shard(symbols: usize) -> SymbolDb {
    let mut db = SymbolDb::new();
    let file = db.files.intern("/src/engine/actor.h");
    let mut scope = None;
    for i in 0..symbols {
        let name = db.intern(&format!("Symbol{i}"));
        let kind = if i % 25 == 0 { SymbolKind::Class } else { SymbolKind::Method };
        let id = db.add_symbol(Symbol::new(name, kind, file, i as u32 * 20).with_parent(if i % 25 == 0 { None } else { scope }));
        if i % 25 == 0 {
            scope = Some(id);
        }
    }
    db
}

fn shards(c: &mut Criterion) {
    let mut g = c.benchmark_group("shard");
    let key = ShardKey { path: "/src/engine/actor.h", content_hash: 1, flags_hash: 2 };
    for &n in &[100usize, 2_000] {
        let db = shard(n);
        let mut bytes = Vec::new();
        encode_shard(&key, &db, &mut bytes);
        g.throughput(Throughput::Bytes(bytes.len() as u64));
        g.bench_with_input(BenchmarkId::new("encode", n), &db, |b, db| {
            let mut buf = Vec::new();
            b.iter(|| encode_shard(&key, black_box(db), &mut buf))
        });
        g.bench_with_input(BenchmarkId::new("decode", n), &bytes, |b, bytes| {
            b.iter(|| decode_shard(black_box(bytes), &key).unwrap().len())
        });
    }
    g.finish();
}

criterion_group!(benches, hashing, compile_commands, shards);
criterion_main!(benches);
