//! Synthetic C++ generator for tests and benchmarks (deterministic per seed).

use std::fmt::Write;

/// xorshift64* — tiny, fast, deterministic.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

const BODIES: &[&str] = &[
    "    for (int i = 0; i < count; ++i) { total += values[i] * 0x1F + 1'000; }\n",
    "    // update transform\n    if (m_dirty && !(flags & 0x4)) { m_world = m_parent->world() * m_local; }\n",
    "    /* multi\n       line */ eastl::vector<eastl::pair<int, float>> tmp; tmp.reserve(16);\n",
    "    const char* msg = \"failed: \\\"%s\\\"\"; Log(msg, name.c_str()); return 1.5e-3f;\n",
    "    auto fn = [this](int x) { return x << 2 >> 1; }; fn(Compute<Foo<Bar>>(x));\n",
];

/// A `compile_commands.json` with `entries` TUs spread over `flag_variants`
/// distinct flag sets. Files are `/work/engine/src/file{i}.cpp`.
pub fn compile_commands_json(entries: usize, flag_variants: usize) -> String {
    let mut s = String::with_capacity(entries * 260);
    s.push('[');
    for i in 0..entries {
        if i > 0 {
            s.push(',');
        }
        let v = i % flag_variants.max(1);
        let _ = write!(
            s,
            "\n{{\"directory\":\"/work/engine/build\",\"file\":\"/work/engine/src/file{i}.cpp\",\
             \"command\":\"clang++ -I/work/engine/include -I../third_party/eastl/include -isystem /usr/include \
             -DENGINE_VARIANT={v} -DNDEBUG -DUNICODE -std=c++20 -c /work/engine/src/file{i}.cpp -o file{i}.o\"}}"
        );
    }
    s.push_str("\n]\n");
    s
}

/// Roughly `target_bytes` of plausible engine-style C++ (classes, virtuals,
/// templates, macros, bodies, comments, raw strings).
pub fn cpp_source(target_bytes: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut s = String::with_capacity(target_bytes + 4096);
    s.push_str("#pragma once\n#include <eastl/vector.h>\n#include \"core/Types.h\"\n\n");
    let mut n = 0usize;
    while s.len() < target_bytes {
        n += 1;
        let ns = format!("engine{}", n % 7);
        let _ = writeln!(s, "namespace {ns} {{\n");
        let _ = writeln!(s, "#if ENGINE_FEATURE_{} && !defined(NO_REFLECTION)", n % 5);
        let _ = writeln!(s, "UCLASS(Blueprintable, meta=(Category=\"Gen{n}\"))");
        let _ = writeln!(s, "#endif");
        let _ = writeln!(s, "/// Generated class {n}.\nclass ENGINE_API Gen{n} : public Base{}, public IFace {{", n % 11);
        s.push_str("public:\n");
        let _ = writeln!(s, "    Gen{n}();\n    virtual ~Gen{n}() override;");
        for m in 0..(4 + rng.below(8)) {
            let virt = if m % 2 == 0 { "virtual " } else { "" };
            let tail = if m % 2 == 0 { " override" } else { " const" };
            let _ = writeln!(s, "    {virt}int Method{m}(float dt, eastl::vector<int>& out){tail}\n    {{");
            for _ in 0..(1 + rng.below(4)) {
                s.push_str(BODIES[rng.below(BODIES.len())]);
            }
            s.push_str("    }\n");
        }
        let _ = writeln!(s, "private:\n    int m_value{n} = {};\n    eastl::vector<Gen{n}*> m_children;", rng.below(1000));
        s.push_str("};\n\n");
        let _ = writeln!(s, "template <typename T, int N = 4>\nstruct Holder{n} {{ T data[N]; }};\n");
        let _ = writeln!(s, "static const char* kDoc{n} = R\"doc(raw ) \"text\" {n})doc\";\n");
        let _ = writeln!(s, "}} // namespace {ns}\n");
    }
    s
}
