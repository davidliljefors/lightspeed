use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use lightspeed::compile_commands::{FlagSet, IncludeDir, IncludeKind};
use lightspeed::pp::vfs::MemFs;
use lightspeed::pp::{PpConfig, PpOutput, Preprocessor};
use lightspeed::testgen::cpp_source;
use rayon::prelude::*;
use std::fmt::Write;
use std::hint::black_box;

fn flags() -> FlagSet {
    FlagSet {
        include_dirs: vec![IncludeDir { path: "/inc".into(), kind: IncludeKind::Normal }],
        std: Some("c++17".into()),
        ..FlagSet::default()
    }
}

/// Engine-like header web: every header is guarded and includes a few others.
fn header_tree(n: usize) -> MemFs {
    let mut fs = MemFs::new();
    for i in 0..n {
        let mut s = format!("#ifndef H{i}_H\n#define H{i}_H\n");
        for k in 1..=3 {
            if i >= k * 2 {
                let _ = writeln!(s, "#include \"h{}.h\"", i - k * 2);
            }
        }
        let _ = writeln!(s, "#define API{i} __declspec(dllexport)\n#if defined(PLATFORM_X) && H{i}_H\n#define PX{i} 1\n#endif");
        s.push_str(&cpp_source(4096, i as u64));
        s.push_str("#endif\n");
        fs.add(&format!("/inc/h{i}.h"), &s);
    }
    fs
}

fn macro_heavy(n: usize) -> String {
    let mut s = String::from(
        "#define CAT_(a,b) a##b\n#define CAT(a,b) CAT_(a,b)\n#define STR_(x) #x\n#define STR(x) STR_(x)\n#define UCLASS(...)\n#define PROP(...) __VA_OPT__(int)\n#define MAX(a,b) ((a)>(b)?(a):(b))\n",
    );
    for i in 0..n {
        let _ = writeln!(s, "UCLASS(Blueprintable, Category = \"x{i}\")\nclass CAT(Foo, {i}) {{ PROP(a, b) CAT(v, {i}) = MAX({i}, {}); const char* n = STR(CAT(Foo, {i})); }};", i + 1);
    }
    s
}

fn preprocess(c: &mut Criterion) {
    let mut g = c.benchmark_group("pp");
    let fl = flags();

    let src = macro_heavy(2000);
    g.throughput(Throughput::Bytes(src.len() as u64));
    let fs = MemFs::new();
    let mut pp = Preprocessor::new(&fs, PpConfig::default());
    pp.set_flags(&fl);
    let mut out = PpOutput::default();
    g.bench_function("macro_heavy", |b| {
        b.iter(|| {
            pp.run("/t/m.cpp", black_box(&src), &mut out);
            out.tokens.len()
        })
    });

    let plain = cpp_source(256 * 1024, 7);
    g.throughput(Throughput::Bytes(plain.len() as u64));
    g.bench_function("plain_256k", |b| {
        b.iter(|| {
            pp.run("/t/p.cpp", black_box(&plain), &mut out);
            out.tokens.len()
        })
    });

    for &n in &[50usize, 400] {
        let fs = header_tree(n);
        let mut pp = Preprocessor::new(&fs, PpConfig { follow_includes: true, ..PpConfig::default() });
        pp.set_flags(&fl);
        let main = format!("#include \"h{}.h\"\n#include \"h{}.h\"\nint x;\n", n - 1, n / 2);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("header_web", n), &main, |b, m| {
            b.iter(|| {
                pp.run("/t/m.cpp", black_box(m), &mut out);
                out.stats.headers_entered
            })
        });
    }
    g.finish();
}

fn scaling(c: &mut Criterion) {
    let mut g = c.benchmark_group("pp_parallel");
    g.sample_size(10);
    let fs = header_tree(200);
    let fl = flags();
    let srcs: Vec<String> = (0..512)
        .map(|i| format!("#include \"h{}.h\"\n#include \"h{}.h\"\n{}", 100 + i % 100, i % 100, macro_heavy(50)))
        .collect();
    g.throughput(Throughput::Elements(srcs.len() as u64));
    let max = rayon::current_num_threads();
    let mut counts = vec![1, 2, 4, 8, 16, 32, 64];
    counts.retain(|&n| n <= max);
    if !counts.contains(&max) {
        counts.push(max);
    }
    for threads in counts {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        // One long-lived Preprocessor per worker thread, as in the real indexer:
        // caches stay warm across iterations. The mutex is never contended.
        let workers: Vec<std::sync::Mutex<(Preprocessor<'_, MemFs>, PpOutput)>> = (0..threads)
            .map(|_| {
                let mut pp = Preprocessor::new(&fs, PpConfig { follow_includes: true, ..PpConfig::default() });
                pp.set_flags(&fl);
                std::sync::Mutex::new((pp, PpOutput::default()))
            })
            .collect();
        g.bench_with_input(BenchmarkId::new("tus", threads), &threads, |b, _| {
            b.iter(|| {
                pool.install(|| {
                    srcs.par_iter()
                        .map(|s| {
                            let i = rayon::current_thread_index().unwrap();
                            let mut guard = workers[i].lock().unwrap();
                            let (pp, o) = &mut *guard;
                            pp.run("/t/m.cpp", s, o);
                            o.tokens.len()
                        })
                        .sum::<usize>()
                })
            })
        });
    }
    g.finish();
}

criterion_group!(benches, preprocess, scaling);
criterion_main!(benches);
