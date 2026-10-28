//! Phase 4: inheritance DAG and Virtual Function Families.
//!
//! [`Hierarchy::build`] runs one linear pass over a (merged) [`SymbolDb`]:
//! resolve base names to class symbols, order classes bases-first, group
//! virtual methods that override each other into *families*, and link
//! out-of-line definitions to their in-class declarations. Afterwards a
//! polymorphic "find references" needs one array lookup:
//! `family_members(method)` is every method that can be dispatched to from a
//! call through any of them.
//!
//! Tolerance trade-offs (speed over C++ correctness):
//! - Overloads are told apart by a parameter signature (arity + hash of
//!   parameter types, see [`crate::symbols::Signature`]). `override` methods
//!   whose spelling differs only by typedef fall back to equal arity.
//! - A method overriding two unrelated bases merges their families.
//! - Base names are resolved by last identifier plus scope proximity, not by
//!   real lookup rules; templates are treated as ordinary classes.
//! - Inheritance cycles are tolerated: classes in a cycle are processed last,
//!   in ID order.

use crate::csr::Csr;
use crate::ids::Idx;
use crate::symbols::{FamilyId, NameId, SymbolDb, SymbolFlags, SymbolId, SymbolKind};
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HierarchyStats {
    pub records: usize,
    pub bases_resolved: usize,
    pub bases_unresolved: usize,
    /// Classes that are part of (or derive from) an inheritance cycle.
    pub cycle_classes: usize,
    pub families: usize,
    pub virtual_methods: usize,
    pub out_of_line_resolved: usize,
    pub out_of_line_unresolved: usize,
}

/// Reused buffers for [`match_out_of_line`].
#[derive(Default)]
struct OutOfLineScratch {
    /// Class a qualifier (`A::B`) names; many definitions share one.
    classes: FxHashMap<NameId, Option<SymbolId>>,
    qname: String,
}

/// The in-class declaration that the out-of-line definition `def` (qualified
/// by `q`, e.g. `A::B`) implements. Best match: same signature, then same
/// arity, then name only; an unclaimed declaration beats a claimed one of
/// equal quality.
fn match_out_of_line(
    db: &SymbolDb,
    def: SymbolId,
    q: NameId,
    scratch: &mut OutOfLineScratch,
    sig: impl Fn(SymbolId) -> Option<(u16, u32)>,
    claimed: impl Fn(SymbolId) -> bool,
) -> Option<SymbolId> {
    let class = (*scratch.classes.entry(q).or_insert_with(|| {
        let qual = db.names.get(q);
        let last = qual.rsplit("::").next().unwrap_or(qual);
        let ln = db.names.lookup(last)?;
        let mut best: Option<(u32, SymbolId)> = None;
        for &c in db.lookup_id(ln) {
            if !db.symbol(c).kind.is_record() {
                continue;
            }
            let qname = &mut scratch.qname;
            qname.clear();
            db.write_qualified_name(c, qname);
            let suffix_ok = qname == qual || (qname.len() >= qual.len() + 2 && qname.ends_with(qual) && qname[..qname.len() - qual.len()].ends_with("::"));
            if !suffix_ok {
                continue;
            }
            let key = u32::from(is_definition(db, c)) + 1;
            if best.is_none_or(|(k, _)| key > k) {
                best = Some((key, c));
            }
        }
        best.map(|(_, c)| c)
    }))?;
    let name = db.symbol(def).name;
    let want = sig(def);
    let mut best: Option<(u32, SymbolId)> = None;
    for &m in db.children(class) {
        if m == def || db.symbol(m).name != name || db.symbol(m).kind == SymbolKind::TypeAlias {
            continue;
        }
        let tier = match (want, sig(m)) {
            (Some(a), Some(b)) if a == b => 3,
            (Some(a), Some(b)) if a.0 == b.0 => 2,
            (Some(_), Some(_)) => 1,
            _ => 3,
        };
        let score = tier * 2 + u32::from(!claimed(m));
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, m));
        }
    }
    best.map(|(_, m)| m)
}

pub struct Hierarchy {
    bases: Csr<SymbolId, SymbolId>,
    derived: Csr<SymbolId, SymbolId>,
    families: Csr<FamilyId, SymbolId>,
    canonical: Vec<Option<SymbolId>>,
    defs: Csr<SymbolId, SymbolId>,
    /// Buckets of the CSRs above rewritten by [`Hierarchy::patch`].
    bases_ovr: FxHashMap<SymbolId, Vec<SymbolId>>,
    derived_ovr: FxHashMap<SymbolId, Vec<SymbolId>>,
    defs_ovr: FxHashMap<SymbolId, Vec<SymbolId>>,
    families_ovr: FxHashMap<FamilyId, Vec<SymbolId>>,
    next_family: u32,
    pub stats: HierarchyStats,
}

impl Hierarchy {
    pub fn heap_bytes(&self) -> usize {
        self.bases.heap_bytes() + self.derived.heap_bytes() + self.families.heap_bytes() + self.defs.heap_bytes() + self.canonical.capacity() * std::mem::size_of::<Option<SymbolId>>()
    }
}

struct Dsu(Vec<u32>);

impl Dsu {
    fn find(&mut self, mut x: u32) -> u32 {
        while self.0[x as usize] != x {
            let p = self.0[x as usize];
            self.0[x as usize] = self.0[p as usize];
            x = p;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            // Smaller ID wins so family numbering is deterministic.
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            self.0[hi as usize] = lo;
        }
    }
}

fn is_definition(db: &SymbolDb, id: SymbolId) -> bool {
    db.symbol(id).flags.contains(SymbolFlags::DEFINITION)
}

/// The class a base-specifier names: last identifier, preferring classes that
/// match the written qualifier, then ones in an enclosing scope of the deriving
/// class (innermost first; scopes are compared by name since a namespace
/// reopened in another file is a different symbol), then definitions, then
/// the deriving class's own file. `chain` is scratch: the names of the
/// deriving class's enclosing scopes, innermost first.
fn resolve_base(db: &SymbolDb, e: &crate::symbols::BaseEdge, chain: &mut Vec<NameId>) -> Option<SymbolId> {
    chain.clear();
    let mut p = db.symbol(e.derived).parent;
    while let Some(x) = p {
        chain.push(db.symbol(x).name);
        p = db.symbol(x).parent;
    }
    let n = chain.len();
    let derived_file = db.symbol(e.derived).file;
    let qual = e.qual.map(|q| db.names.get(q));
    let mut best: Option<(u32, SymbolId)> = None;
    for &c in db.lookup_id(e.base_name) {
        let s = db.symbol(c);
        if !s.kind.is_record() || c == e.derived {
            continue;
        }
        let mut m = 0usize;
        let mut p = s.parent;
        while let Some(x) = p {
            m += 1;
            p = db.symbol(x).parent;
        }
        let qual_ok = qual.is_some_and(|q| {
            let mut scope = s.parent;
            let mut segs = if q.is_empty() { None } else { Some(q.rsplit("::")) };
            let mut ok = true;
            if let Some(it) = segs.as_mut() {
                for seg in it {
                    match scope {
                        Some(x) if db.names.get(db.symbol(x).name) == seg => scope = db.symbol(x).parent,
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                ok
            } else {
                m == 0
            }
        });
        let visible = m <= n && {
            let mut p = s.parent;
            let mut k = n - m;
            let mut same = true;
            while let Some(x) = p {
                same &= db.symbol(x).name == chain[k];
                k += 1;
                p = db.symbol(x).parent;
            }
            same
        };
        let key = (u32::from(qual_ok) << 26)
            | (u32::from(visible) << 25)
            | ((if visible { m.min(255) as u32 } else { 0 }) << 17)
            | (u32::from(is_definition(db, c)) << 16)
            | (u32::from(s.file == derived_file) << 15);
        if best.is_none_or(|(k, _)| key > k) {
            best = Some((key, c));
        }
    }
    best.map(|(_, c)| c)
}

/// Union-find over a handful of symbols; the smaller ID is the root.
#[derive(Default)]
struct MapDsu(FxHashMap<SymbolId, SymbolId>);

impl MapDsu {
    fn find(&mut self, x: SymbolId) -> SymbolId {
        let mut r = x;
        while let Some(&p) = self.0.get(&r) {
            if p == r {
                break;
            }
            r = p;
        }
        let mut y = x;
        while y != r {
            let p = self.0[&y];
            self.0.insert(y, r);
            y = p;
        }
        r
    }

    fn union(&mut self, a: SymbolId, b: SymbolId) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            self.0.insert(hi, lo);
        }
    }
}

impl Hierarchy {
    /// Resolves bases (writing `BaseEdge::base` into `db`), assigns
    /// `Symbol::family`, and builds the graph indexes. Rebuilds the name and
    /// children indexes first if they are stale. O(symbols + edges) plus a
    /// same-name scan per method of a derived class.
    pub fn build(db: &mut SymbolDb) -> Self {
        if !db.indexes_fresh() {
            db.rebuild_indexes();
        }
        let n = db.len();
        let mut stats = HierarchyStats::default();

        // 1. Resolve base names.
        let mut resolved: Vec<Option<SymbolId>> = Vec::with_capacity(db.bases().len());
        let mut chain: Vec<NameId> = Vec::new();
        for e in db.bases() {
            resolved.push(resolve_base(db, e, &mut chain));
        }
        for (e, r) in db.bases_mut().iter_mut().zip(&resolved) {
            e.base = *r;
        }
        stats.bases_resolved = resolved.iter().flatten().count();
        stats.bases_unresolved = resolved.len() - stats.bases_resolved;

        let edges = || db.bases().iter().filter_map(|e| e.base.map(|b| (e.derived, b)));
        let bases = Csr::build(n, edges());
        let derived = Csr::build(n, edges().map(|(d, b)| (b, d)));

        // 2. Classes in bases-first order (Kahn); cycle members go last.
        let mut indeg = vec![0u32; n];
        let mut order: Vec<SymbolId> = Vec::new();
        let mut is_rec = vec![false; n];
        for (id, s) in db.symbols().iter_enumerated() {
            if s.kind.is_record() && !s.flags.contains(SymbolFlags::DEAD) {
                is_rec[id.index()] = true;
                stats.records += 1;
                indeg[id.index()] = bases.get(id).len() as u32;
                if indeg[id.index()] == 0 {
                    order.push(id);
                }
            }
        }
        let mut head = 0;
        while head < order.len() {
            let c = order[head];
            head += 1;
            for &d in derived.get(c) {
                let i = d.index();
                indeg[i] -= 1;
                if indeg[i] == 0 {
                    order.push(d);
                }
            }
        }
        if order.len() < stats.records {
            let mut done = vec![false; n];
            for c in &order {
                done[c.index()] = true;
            }
            for (i, &r) in is_rec.iter().enumerate() {
                if r && !done[i] {
                    order.push(SymbolId::from_usize(i));
                    stats.cycle_classes += 1;
                }
            }
        }

        // 3. Virtual families.
        let mut sig: Vec<Option<(u16, u32)>> = vec![None; n];
        for s in db.signatures() {
            sig[s.symbol.index()] = Some((s.arity, s.hash));
        }
        let mut arity_cands: Vec<SymbolId> = Vec::new();
        let mut dsu = Dsu((0..n as u32).collect());
        let mut eff = vec![false; n];
        let mut dtor_of: Vec<Option<SymbolId>> = vec![None; n];
        for (id, s) in db.symbols().iter_enumerated() {
            if s.kind == SymbolKind::Destructor
                && let Some(p) = s.parent
            {
                dtor_of[p.index()].get_or_insert(id);
            }
        }
        let mut mark = vec![0u32; n];
        // Virtual methods found so far, by name: an override candidate is one of these.
        let mut eff_by_name: FxHashMap<NameId, Vec<SymbolId>> = FxHashMap::default();
        let (mut stamp, mut anc, mut stack) = (0u32, Vec::<SymbolId>::new(), Vec::<SymbolId>::new());
        for &c in &order {
            stamp += 1;
            anc.clear();
            stack.clear();
            mark[c.index()] = stamp;
            stack.push(c);
            while let Some(x) = stack.pop() {
                for &b in bases.get(x) {
                    if mark[b.index()] != stamp {
                        mark[b.index()] = stamp;
                        anc.push(b);
                        stack.push(b);
                    }
                }
            }
            for &m in db.children(c) {
                let s = db.symbol(m);
                if s.flags.contains(SymbolFlags::STATIC) {
                    continue;
                }
                let declared = s.flags.is_virtual_like();
                let mut found = false;
                match s.kind {
                    SymbolKind::Destructor => {
                        for &a in &anc {
                            if let Some(d) = dtor_of[a.index()]
                                && eff[d.index()]
                            {
                                dsu.union(m.index() as u32, d.index() as u32);
                                found = true;
                            }
                        }
                    }
                    SymbolKind::Method if !anc.is_empty() => {
                        // Same name and same parameter types override each other.
                        // If the method says `override` but no spelling matches
                        // exactly (typedef vs. alias), fall back to equal arity.
                        arity_cands.clear();
                        let mine = sig[m.index()];
                        for &o in eff_by_name.get(&s.name).map_or(&[][..], |v| &v[..]) {
                            let os = db.symbol(o);
                            if os.kind == SymbolKind::Method
                                && let Some(p) = os.parent
                                && p != c
                                && mark[p.index()] == stamp
                            {
                                match (mine, sig[o.index()]) {
                                    (Some(a), Some(b)) if a != b => {
                                        if a.0 == b.0 {
                                            arity_cands.push(o);
                                        }
                                    }
                                    _ => {
                                        dsu.union(m.index() as u32, o.index() as u32);
                                        found = true;
                                    }
                                }
                            }
                        }
                        if !found && s.flags.contains(SymbolFlags::OVERRIDE) {
                            for &o in &arity_cands {
                                dsu.union(m.index() as u32, o.index() as u32);
                                found = true;
                            }
                        }
                    }
                    SymbolKind::Method => {}
                    _ => continue,
                }
                eff[m.index()] = declared || found;
                if eff[m.index()] {
                    eff_by_name.entry(s.name).or_default().push(m);
                }
            }
        }

        let mut fam_of_root = vec![u32::MAX; n];
        let mut fam_of: Vec<Option<FamilyId>> = vec![None; n];
        let mut nfam = 0usize;
        for i in 0..n {
            if eff[i] {
                stats.virtual_methods += 1;
                let r = dsu.find(i as u32) as usize;
                if fam_of_root[r] == u32::MAX {
                    fam_of_root[r] = nfam as u32;
                    nfam += 1;
                }
                fam_of[i] = Some(FamilyId::from_usize(fam_of_root[r] as usize));
            }
        }
        stats.families = nfam;

        // 4. Out-of-line definitions -> in-class declarations.
        let mut canonical: Vec<Option<SymbolId>> = vec![None; n];
        let mut claimed = vec![false; n];
        let mut scratch = OutOfLineScratch::default();
        for &(def, q) in db.out_of_line() {
            let member = match_out_of_line(db, def, q, &mut scratch, |x| sig[x.index()], |m| claimed[m.index()]);
            match member {
                Some(m) => {
                    claimed[m.index()] = true;
                    canonical[def.index()] = Some(m);
                    fam_of[def.index()] = fam_of[m.index()];
                    stats.out_of_line_resolved += 1;
                }
                None => stats.out_of_line_unresolved += 1,
            }
        }

        let families = Csr::build(nfam, fam_of.iter().enumerate().filter_map(|(i, f)| f.map(|f| (f, SymbolId::from_usize(i)))));
        for (i, f) in fam_of.iter().enumerate() {
            db.symbol_mut(SymbolId::from_usize(i)).family = *f;
        }

        let defs = Csr::build(n, canonical.iter().enumerate().filter_map(|(i, c)| c.map(|c| (c, SymbolId::from_usize(i)))));
        Self {
            bases,
            derived,
            families,
            canonical,
            defs,
            bases_ovr: FxHashMap::default(),
            derived_ovr: FxHashMap::default(),
            defs_ovr: FxHashMap::default(),
            families_ovr: FxHashMap::default(),
            next_family: nfam as u32,
            stats,
        }
    }

    /// Updates the hierarchy after the file whose symbols were `old` was
    /// tombstoned and re-merged as `new` (name/children indexes already
    /// patched). Only the classes related to the file's classes (its
    /// inheritance component) are recomputed, and out-of-line definitions are
    /// re-matched individually, so the cost does not depend on database size.
    pub fn patch(&mut self, db: &mut SymbolDb, old: std::ops::Range<usize>, new: std::ops::Range<usize>) {
        self.canonical.resize(db.len(), None);
        for i in old.clone() {
            let def = SymbolId::from_usize(i);
            let Some(m) = self.canonical[i].take() else { continue };
            self.defs_vec(m).retain(|&d| d != def);
            if let Some(f) = db.symbol(def).family {
                self.families_vec(f).retain(|&d| d != def);
            }
        }
        let records = |r: &std::ops::Range<usize>, db: &SymbolDb| -> Vec<SymbolId> {
            r.clone().map(SymbolId::from_usize).filter(|&s| db.symbol(s).kind.is_record()).collect()
        };
        let (old_recs, new_recs) = (records(&old, db), records(&new, db));
        if !old_recs.is_empty() || !new_recs.is_empty() {
            self.patch_classes(db, &old_recs, &new_recs, &new);
        }
        let mut scratch = OutOfLineScratch::default();
        let first = db.out_of_line().partition_point(|e| e.0.index() < new.start);
        let added: Vec<(SymbolId, NameId)> = db.out_of_line()[first..].to_vec();
        for (def, q) in added {
            self.match_def(db, def, q, &mut scratch);
        }
        // Definitions elsewhere whose class was replaced, or that can now find theirs.
        if !old_recs.is_empty() || !new_recs.is_empty() {
            let new_names: FxHashSet<NameId> = new_recs.iter().map(|&r| db.symbol(r).name).collect();
            let mut relevant: FxHashMap<NameId, bool> = FxHashMap::default();
            let todo: Vec<(SymbolId, NameId)> = db.out_of_line()[..first]
                .iter()
                .copied()
                .filter(|&(def, q)| match self.canonical[def.index()] {
                    Some(m) => db.is_dead(m),
                    None => *relevant.entry(q).or_insert_with(|| {
                        let qual = db.names.get(q);
                        db.names.lookup(qual.rsplit("::").next().unwrap_or(qual)).is_some_and(|n| new_names.contains(&n))
                    }),
                })
                .collect();
            for (def, q) in todo {
                self.canonical[def.index()] = None;
                if let Some(f) = db.symbol_mut(def).family.take() {
                    self.families_vec(f).retain(|&d| d != def);
                }
                self.match_def(db, def, q, &mut scratch);
            }
        }
    }

    fn match_def(&mut self, db: &mut SymbolDb, def: SymbolId, q: NameId, scratch: &mut OutOfLineScratch) {
        let found = match_out_of_line(db, def, q, scratch, |x| db.signature(x).map(|s| (s.arity, s.hash)), |m| !self.out_of_line_defs(m).is_empty());
        let Some(m) = found else { return };
        self.canonical[def.index()] = Some(m);
        self.defs_vec(m).push(def);
        let fam = db.symbol(m).family;
        db.symbol_mut(def).family = fam;
        if let Some(f) = fam {
            self.families_vec(f).push(def);
        }
    }

    fn defs_vec(&mut self, m: SymbolId) -> &mut Vec<SymbolId> {
        self.defs_ovr.entry(m).or_insert_with(|| self.defs.get(m).to_vec())
    }

    fn families_vec(&mut self, f: FamilyId) -> &mut Vec<SymbolId> {
        self.families_ovr.entry(f).or_insert_with(|| self.families.get(f).to_vec())
    }

    fn derived_vec(&mut self, b: SymbolId) -> &mut Vec<SymbolId> {
        self.derived_ovr.entry(b).or_insert_with(|| self.derived.get(b).to_vec())
    }

    /// Class edges, the component of related classes, and its families.
    fn patch_classes(&mut self, db: &mut SymbolDb, old_recs: &[SymbolId], new_recs: &[SymbolId], new: &std::ops::Range<usize>) {
        let live_rec = |db: &SymbolDb, c: SymbolId| !db.is_dead(c) && db.symbol(c).kind.is_record();
        let mut seeds: FxHashSet<SymbolId> = new_recs.iter().copied().collect();
        for &r in old_recs {
            for b in self.bases(r).to_vec() {
                self.derived_vec(b).retain(|&d| d != r);
                seeds.insert(b);
            }
            seeds.extend(self.derived(r).iter().copied());
            self.bases_ovr.insert(r, Vec::new());
            self.derived_ovr.insert(r, Vec::new());
        }

        // Re-resolve every base specifier that can have changed.
        let new_names: FxHashSet<NameId> = new_recs.iter().map(|&r| db.symbol(r).name).collect();
        let mut chain = Vec::new();
        let mut updates: Vec<(usize, Option<SymbolId>)> = Vec::new();
        let mut changed: FxHashSet<SymbolId> = FxHashSet::default();
        for (i, e) in db.bases().iter().enumerate() {
            let fresh = new.contains(&e.derived.index());
            if !(fresh || e.base.is_some_and(|b| db.is_dead(b)) || new_names.contains(&e.base_name)) {
                continue;
            }
            let r = resolve_base(db, e, &mut chain);
            if fresh || r != e.base {
                updates.push((i, r));
                changed.insert(e.derived);
            }
        }
        for (i, r) in updates {
            db.bases_mut()[i].base = r;
        }
        if !changed.is_empty() {
            let mut lists: FxHashMap<SymbolId, Vec<SymbolId>> = changed.iter().map(|&d| (d, Vec::new())).collect();
            for e in db.bases() {
                if let (Some(l), Some(b)) = (lists.get_mut(&e.derived), e.base) {
                    l.push(b);
                }
            }
            for (d, list) in lists {
                for b in self.bases(d).to_vec() {
                    self.derived_vec(b).retain(|&x| x != d);
                }
                for &b in &list {
                    self.derived_vec(b).push(d);
                }
                self.bases_ovr.insert(d, list);
                seeds.insert(d);
            }
        }

        // The classes connected to the changed ones are the only ones whose families can differ.
        let mut comp: Vec<SymbolId> = Vec::new();
        let mut seen: FxHashSet<SymbolId> = FxHashSet::default();
        let mut stack: Vec<SymbolId> = seeds.into_iter().filter(|&c| live_rec(db, c)).collect();
        while let Some(c) = stack.pop() {
            if !seen.insert(c) {
                continue;
            }
            comp.push(c);
            stack.extend(self.bases(c).iter().chain(self.derived(c)).copied().filter(|&x| live_rec(db, x) && !seen.contains(&x)));
        }
        comp.sort_unstable();
        self.recompute_families(db, &comp);
    }

    /// Redoes the virtual families of `comp`, a set of classes closed under
    /// bases and derived classes (the same algorithm as [`Self::build`]).
    fn recompute_families(&mut self, db: &mut SymbolDb, comp: &[SymbolId]) {
        let members: Vec<SymbolId> = comp.iter().flat_map(|&c| db.children(c).iter().copied()).collect();
        // Definitions attached to a declaration elsewhere take its family; they are not members here.
        let is_def = |h: &Self, m: SymbolId| h.canonical.get(m.index()).copied().flatten().is_some_and(|c| c != m);
        let members: Vec<SymbolId> = members.into_iter().filter(|&m| !is_def(self, m)).collect();
        for &m in &members {
            if let Some(f) = db.symbol(m).family {
                self.families_ovr.insert(f, Vec::new());
                db.symbol_mut(m).family = None;
                for d in self.out_of_line_defs(m).to_vec() {
                    db.symbol_mut(d).family = None;
                }
            }
        }

        let mut indeg: FxHashMap<SymbolId, u32> = comp.iter().map(|&c| (c, self.bases(c).len() as u32)).collect();
        let mut order: Vec<SymbolId> = comp.iter().copied().filter(|c| indeg[c] == 0).collect();
        let mut head = 0;
        while head < order.len() {
            let c = order[head];
            head += 1;
            for &d in self.derived(c) {
                if let Some(x) = indeg.get_mut(&d)
                    && *x > 0
                {
                    *x -= 1;
                    if *x == 0 {
                        order.push(d);
                    }
                }
            }
        }
        if order.len() < comp.len() {
            let done: FxHashSet<SymbolId> = order.iter().copied().collect();
            order.extend(comp.iter().copied().filter(|c| !done.contains(c)));
        }

        let mut dtor_of: FxHashMap<SymbolId, SymbolId> = FxHashMap::default();
        for &c in comp {
            if let Some(&d) = db.children(c).iter().find(|&&m| db.symbol(m).kind == SymbolKind::Destructor) {
                dtor_of.insert(c, d);
            }
        }
        let mut dsu = MapDsu::default();
        let mut eff: FxHashSet<SymbolId> = FxHashSet::default();
        let mut eff_by_name: FxHashMap<NameId, Vec<SymbolId>> = FxHashMap::default();
        let (mut anc, mut anc_set, mut stack) = (Vec::<SymbolId>::new(), FxHashSet::<SymbolId>::default(), Vec::<SymbolId>::new());
        let mut arity_cands: Vec<SymbolId> = Vec::new();
        for &c in &order {
            anc.clear();
            anc_set.clear();
            stack.clear();
            anc_set.insert(c);
            stack.push(c);
            while let Some(x) = stack.pop() {
                for &b in self.bases(x) {
                    if anc_set.insert(b) {
                        anc.push(b);
                        stack.push(b);
                    }
                }
            }
            for &m in db.children(c) {
                let s = db.symbol(m);
                if is_def(self, m) {
                    continue;
                }
                if s.flags.contains(SymbolFlags::STATIC) {
                    continue;
                }
                let declared = s.flags.is_virtual_like();
                let mut found = false;
                match s.kind {
                    SymbolKind::Destructor => {
                        for a in &anc {
                            if let Some(&d) = dtor_of.get(a)
                                && eff.contains(&d)
                            {
                                dsu.union(m, d);
                                found = true;
                            }
                        }
                    }
                    SymbolKind::Method if !anc.is_empty() => {
                        arity_cands.clear();
                        let mine = db.signature(m).map(|s| (s.arity, s.hash));
                        for &o in eff_by_name.get(&s.name).map_or(&[][..], |v| &v[..]) {
                            let os = db.symbol(o);
                            if os.kind == SymbolKind::Method
                                && let Some(p) = os.parent
                                && p != c
                                && anc_set.contains(&p)
                            {
                                match (mine, db.signature(o).map(|s| (s.arity, s.hash))) {
                                    (Some(a), Some(b)) if a != b => {
                                        if a.0 == b.0 {
                                            arity_cands.push(o);
                                        }
                                    }
                                    _ => {
                                        dsu.union(m, o);
                                        found = true;
                                    }
                                }
                            }
                        }
                        if !found && s.flags.contains(SymbolFlags::OVERRIDE) {
                            for &o in &arity_cands {
                                dsu.union(m, o);
                                found = true;
                            }
                        }
                    }
                    SymbolKind::Method => {}
                    _ => continue,
                }
                if declared || found {
                    eff.insert(m);
                    eff_by_name.entry(s.name).or_default().push(m);
                }
            }
        }

        let mut eff: Vec<SymbolId> = eff.into_iter().collect();
        eff.sort_unstable();
        let mut fam_of_root: FxHashMap<SymbolId, FamilyId> = FxHashMap::default();
        for m in eff {
            let root = dsu.find(m);
            let f = *fam_of_root.entry(root).or_insert_with(|| {
                self.next_family += 1;
                FamilyId::from_usize(self.next_family as usize - 1)
            });
            db.symbol_mut(m).family = Some(f);
            self.families_ovr.entry(f).or_default().push(m);
            for d in self.out_of_line_defs(m).to_vec() {
                db.symbol_mut(d).family = Some(f);
                self.families_ovr.entry(f).or_default().push(d);
            }
        }
    }

    /// Out-of-line definitions of the in-class declaration `decl`.
    pub fn out_of_line_defs(&self, decl: SymbolId) -> &[SymbolId] {
        match self.defs_ovr.get(&decl) {
            Some(l) => l,
            None => self.defs.get(decl),
        }
    }

    /// Direct, resolved base classes.
    pub fn bases(&self, class: SymbolId) -> &[SymbolId] {
        match self.bases_ovr.get(&class) {
            Some(l) => l,
            None => self.bases.get(class),
        }
    }

    /// Direct derived classes.
    pub fn derived(&self, class: SymbolId) -> &[SymbolId] {
        match self.derived_ovr.get(&class) {
            Some(l) => l,
            None => self.derived.get(class),
        }
    }

    fn closure(&self, down: bool, start: SymbolId, out: &mut Vec<SymbolId>) {
        let step = |x| if down { self.derived(x) } else { self.bases(x) };
        let from = out.len();
        let mut seen: FxHashSet<SymbolId> = FxHashSet::default();
        seen.insert(start);
        let mut i = from;
        out.extend(step(start).iter().copied().filter(|&x| seen.insert(x)));
        while i < out.len() {
            let x = out[i];
            i += 1;
            for &y in step(x) {
                if seen.insert(y) {
                    out.push(y);
                }
            }
        }
    }

    /// Appends every transitive base of `class` (nearest first) to `out`.
    pub fn all_bases(&self, class: SymbolId, out: &mut Vec<SymbolId>) {
        self.closure(false, class, out);
    }

    /// Appends every transitive derived class of `class` to `out`.
    pub fn all_derived(&self, class: SymbolId, out: &mut Vec<SymbolId>) {
        self.closure(true, class, out);
    }

    /// True if `derived` is `base` or inherits from it.
    pub fn is_a(&self, derived: SymbolId, base: SymbolId) -> bool {
        if derived == base {
            return true;
        }
        let mut v = Vec::new();
        self.all_bases(derived, &mut v);
        v.contains(&base)
    }

    /// Every method in the same virtual family as `m` (including `m` and its
    /// out-of-line definitions). Empty if `m` is not virtual. O(1).
    pub fn family_members(&self, db: &SymbolDb, m: SymbolId) -> &[SymbolId] {
        db.symbol(m).family.map_or(&[], |f| self.family(f))
    }

    pub fn family(&self, family: FamilyId) -> &[SymbolId] {
        match self.families_ovr.get(&family) {
            Some(l) => l,
            None => self.families.get(family),
        }
    }

    /// For an out-of-line definition, the in-class declaration; otherwise `sym`.
    pub fn canonical(&self, sym: SymbolId) -> SymbolId {
        self.canonical.get(sym.index()).copied().flatten().unwrap_or(sym)
    }

    /// Methods of strict base classes that `m` overrides (same family).
    pub fn overridden(&self, db: &SymbolDb, m: SymbolId, out: &mut Vec<SymbolId>) {
        let Some(class) = db.symbol(self.canonical(m)).parent else { return };
        let mut anc = Vec::new();
        self.all_bases(class, &mut anc);
        out.extend(self.family_members(db, m).iter().copied().filter(|&x| {
            let p = db.symbol(self.canonical(x)).parent;
            x != m && p.is_some_and(|p| anc.contains(&p))
        }));
    }

    /// Methods of strict derived classes that override `m` (same family).
    pub fn overriders(&self, db: &SymbolDb, m: SymbolId, out: &mut Vec<SymbolId>) {
        let Some(class) = db.symbol(self.canonical(m)).parent else { return };
        let mut des = Vec::new();
        self.all_derived(class, &mut des);
        out.extend(self.family_members(db, m).iter().copied().filter(|&x| {
            let p = db.symbol(self.canonical(x)).parent;
            x != m && p.is_some_and(|p| des.contains(&p))
        }));
    }
}
