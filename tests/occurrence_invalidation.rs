//! How much of the resolved-occurrence table survives an edit? Run on a Godot checkout:
//! `cargo test --release --test occurrence_invalidation -- --ignored --nocapture`

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use lightspeed::query::{DiskText, Query, Text, TextSource};
use lightspeed::workspace::compile_commands::CompileDb;
use lightspeed::workspace::index::Index;

const ROOT: &str = "C:/dev/godot";

struct Ov(HashMap<String, Arc<str>>);

impl TextSource for Ov {
    fn text(&self, path: &str) -> Option<Text> {
        match self.0.get(path) {
            Some(t) => Some(Text::Shared(t.clone())),
            None => DiskText.text(path),
        }
    }
}

type Snapshot = HashMap<String, Vec<(u32, u64)>>;

static NAMES: std::sync::Mutex<std::sync::LazyLock<HashMap<u64, String>>> = std::sync::Mutex::new(std::sync::LazyLock::new(HashMap::new));

fn snapshot(idx: &Index, ov: &Ov) -> Snapshot {
    let q = Query::new(idx, ov);
    let (occ, _) = q.build_occurrences();
    let mut desc: HashMap<lightspeed::symbols::SymbolId, u64> = HashMap::new();
    let mut out = Snapshot::new();
    for (fi, f) in idx.live_files() {
        let sites = occ[fi]
            .sites
            .iter()
            .map(|&(o, s)| {
                let h = *desc.entry(s).or_insert_with(|| {
                    let mut h = DefaultHasher::new();
                    // Which member of a family is canonical depends on the hierarchy build; the family is the entity.
                    let d = q.entity_records(s).into_iter().map(|r| format!("{} {:?} {}", idx.db.qualified_name(r), idx.db.symbol(r).kind, idx.db.file_path(r))).min().unwrap();
                    d.hash(&mut h);
                    let k = h.finish();
                    NAMES.lock().unwrap().insert(k, d);
                    k
                });
                (o, h)
            })
            .collect();
        out.insert(f.path.clone(), sites);
    }
    out
}

fn diff(base: &Snapshot, now: &Snapshot, edited: &str) {
    let (mut files, mut changed, mut total, mut lost, mut gained) = (0, 0usize, 0usize, 0usize, 0usize);
    let mut worst: Vec<(usize, &str)> = Vec::new();
    for (p, b) in base {
        if p == edited {
            continue;
        }
        total += b.len();
        let n = now.get(p).map_or(&[][..], |v| v.as_slice());
        if b.as_slice() == n {
            continue;
        }
        let bm: HashMap<_, _> = b.iter().copied().collect();
        let nm: HashMap<_, _> = n.iter().copied().collect();
        let l = bm.iter().filter(|(o, h)| nm.get(*o) != Some(*h)).count();
        let g = nm.iter().filter(|(o, h)| bm.get(*o) != Some(*h) && !bm.contains_key(*o)).count();
        files += 1;
        changed += l + g;
        lost += bm.iter().filter(|(o, _)| !nm.contains_key(*o)).count();
        gained += g;
        worst.push((l + g, p));
    }
    worst.sort_unstable_by(|a, b| b.cmp(a));
    println!(
        "  other files: {files} of {} changed; {changed} of {total} entries differ ({:.4}%) [lost {lost}, gained {gained}, rewritten {}]",
        base.len(),
        100.0 * changed as f64 / total as f64,
        changed - lost - gained
    );
    for (n, p) in worst.iter().take(3) {
        println!("    {n} in {p}");
        let b: HashMap<_, _> = base[*p].iter().copied().collect();
        let names = NAMES.lock().unwrap();
        for &(o, h) in now[*p].iter().filter(|(o, h)| b.get(o).is_some_and(|x| x != h)).take(3) {
            println!("      @{o}: {} -> {}", names[&b[&o]], names[&h]);
        }
    }
    let (b, n) = (base.get(edited).map_or(0, Vec::len), now.get(edited).map_or(0, Vec::len));
    println!("  edited file itself: {b} -> {n} entries");
}

type Edit = Box<dyn Fn(&str) -> String>;

#[test]
#[ignore = "needs a Godot checkout and --release"]
fn godot_edit_invalidation() {
    let mut cdb = CompileDb::load(format!("{ROOT}/compile_commands.json")).unwrap();
    cdb.add_system_includes();
    let mut idx = Index::build(cdb, Default::default());
    let t = std::time::Instant::now();
    let base = snapshot(&idx, &Ov(HashMap::new()));
    println!("baseline table in {:.1} s", t.elapsed().as_secs_f64());

    let read = |p: &str| std::fs::read_to_string(format!("{ROOT}/{p}")).unwrap();
    let scenarios: Vec<(&str, &str, Edit)> = vec![
        ("comment at top of a .cpp (offsets shift, no symbols change)", "scene/main/node.cpp", Box::new(|t| format!("// probe\n{t}"))),
        ("new free function appended to a .cpp", "scene/main/node.cpp", Box::new(|t| format!("{t}\nint zz_probe_function(int a) {{ return a; }}\n"))),
        ("new #include at top of a .cpp", "scene/main/node.cpp", Box::new(|t| format!("#include \"core/os/os.h\"\n{t}"))),
        (
            "unrelated new method added to class Node (widely included header)",
            "scene/main/node.h",
            Box::new(|t| t.replacen("class Node : public Object {\n", "class Node : public Object {\npublic:\n\tvoid zz_probe_method();\n", 1)),
        ),
        (
            "new overload push_back(int) added to Vector",
            "core/templates/vector.h",
            Box::new(|t| t.replacen("_FORCE_INLINE_ bool push_back(", "_FORCE_INLINE_ bool push_back(int zz_probe, int zz_probe2);\n\t_FORCE_INLINE_ bool push_back(", 1)),
        ),
    ];
    for (what, file, edit) in scenarios {
        let path = format!("{ROOT}/{file}");
        let orig = read(file);
        let new = edit(&orig);
        assert_ne!(orig, new, "edit did not apply: {what}");
        println!("\n{what}");
        let ov = Ov(HashMap::from([(path.clone(), Arc::from(new.as_str()))]));
        idx.update(&path, Some(&new));
        idx.flush();
        let t = std::time::Instant::now();
        let now = snapshot(&idx, &ov);
        println!("  rebuilt in {:.1} s", t.elapsed().as_secs_f64());
        diff(&base, &now, &path);
        idx.update(&path, Some(&orig));
        idx.flush();
    }
}
