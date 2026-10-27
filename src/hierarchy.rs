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
use crate::symbols::{FamilyId, SymbolDb, SymbolFlags, SymbolId, SymbolKind};
use rustc_hash::FxHashSet;

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

pub struct Hierarchy {
    bases: Csr<SymbolId, SymbolId>,
    derived: Csr<SymbolId, SymbolId>,
    families: Csr<FamilyId, SymbolId>,
    canonical: Vec<Option<SymbolId>>,
    defs: Csr<SymbolId, SymbolId>,
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
        let mut chain: Vec<SymbolId> = Vec::new();
        for e in db.bases() {
            chain.clear();
            let mut p = db.symbol(e.derived).parent;
            while let Some(x) = p {
                chain.push(x);
                p = db.symbol(x).parent;
            }
            let mut best: Option<(u32, SymbolId)> = None;
            for &c in db.lookup_id(e.base_name) {
                let s = db.symbol(c);
                if !s.kind.is_record() || c == e.derived {
                    continue;
                }
                let mut prox = 0u32;
                let mut p = s.parent;
                while let Some(x) = p {
                    prox += chain.contains(&x) as u32;
                    p = db.symbol(x).parent;
                }
                let key = (u32::from(is_definition(db, c)) << 16) | prox.min(0xffff);
                if best.is_none_or(|(k, _)| key > k) {
                    best = Some((key, c));
                }
            }
            resolved.push(best.map(|(_, c)| c));
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
                        for &o in db.lookup_id(s.name) {
                            if o == m || !eff[o.index()] {
                                continue;
                            }
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
        let mut qname = String::new();
        for &(def, q) in db.out_of_line() {
            let qual = db.names.get(q);
            let last = qual.rsplit("::").next().unwrap_or(qual);
            let class = db.names.lookup(last).and_then(|ln| {
                let mut best: Option<(u32, SymbolId)> = None;
                for &c in db.lookup_id(ln) {
                    if !db.symbol(c).kind.is_record() {
                        continue;
                    }
                    qname.clear();
                    db.write_qualified_name(c, &mut qname);
                    let suffix_ok = qname == qual
                        || (qname.len() >= qual.len() + 2 && qname.ends_with(qual) && qname[..qname.len() - qual.len()].ends_with("::"));
                    if !suffix_ok {
                        continue;
                    }
                    let key = u32::from(is_definition(db, c)) + 1;
                    if best.is_none_or(|(k, _)| key > k) {
                        best = Some((key, c));
                    }
                }
                best.map(|(_, c)| c)
            });
            let name = db.symbol(def).name;
            let want = sig[def.index()];
            let member = class.and_then(|c| {
                // Best match: same signature, then same arity, then name only;
                // an unclaimed declaration beats a claimed one of equal quality.
                let mut best: Option<(u32, SymbolId)> = None;
                for &m in db.children(c) {
                    if m == def || db.symbol(m).name != name || db.symbol(m).kind == SymbolKind::TypeAlias {
                        continue;
                    }
                    let tier = match (want, sig[m.index()]) {
                        (Some(a), Some(b)) if a == b => 3,
                        (Some(a), Some(b)) if a.0 == b.0 => 2,
                        (Some(_), Some(_)) => 1,
                        _ => 3,
                    };
                    let score = tier * 2 + u32::from(!claimed[m.index()]);
                    if best.is_none_or(|(s, _)| score > s) {
                        best = Some((score, m));
                    }
                }
                best.map(|(_, m)| m)
            });
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
        Self { bases, derived, families, canonical, defs, stats }
    }

    /// Out-of-line definitions of the in-class declaration `decl`.
    pub fn out_of_line_defs(&self, decl: SymbolId) -> &[SymbolId] {
        if decl.index() < self.defs.num_keys() { self.defs.get(decl) } else { &[] }
    }

    /// Direct, resolved base classes.
    pub fn bases(&self, class: SymbolId) -> &[SymbolId] {
        self.bases.get(class)
    }

    /// Direct derived classes.
    pub fn derived(&self, class: SymbolId) -> &[SymbolId] {
        self.derived.get(class)
    }

    fn closure(csr: &Csr<SymbolId, SymbolId>, start: SymbolId, out: &mut Vec<SymbolId>) {
        let from = out.len();
        let mut seen: FxHashSet<SymbolId> = FxHashSet::default();
        seen.insert(start);
        let mut i = from;
        out.extend(csr.get(start).iter().copied().filter(|&x| seen.insert(x)));
        while i < out.len() {
            let x = out[i];
            i += 1;
            for &y in csr.get(x) {
                if seen.insert(y) {
                    out.push(y);
                }
            }
        }
    }

    /// Appends every transitive base of `class` (nearest first) to `out`.
    pub fn all_bases(&self, class: SymbolId, out: &mut Vec<SymbolId>) {
        Self::closure(&self.bases, class, out);
    }

    /// Appends every transitive derived class of `class` to `out`.
    pub fn all_derived(&self, class: SymbolId, out: &mut Vec<SymbolId>) {
        Self::closure(&self.derived, class, out);
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
        db.symbol(m).family.map_or(&[], |f| self.families.get(f))
    }

    pub fn family(&self, family: FamilyId) -> &[SymbolId] {
        self.families.get(family)
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
