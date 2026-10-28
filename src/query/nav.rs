//! `textDocument/declaration`, `implementation` and `typeDefinition`.

use super::{Cx, Ty, declared_type_tail};
use crate::query::{Query, sig_tokens};
use crate::symbols::{SymbolFlags, SymbolId, SymbolKind};

fn push_new(out: &mut Vec<SymbolId>, id: SymbolId) {
    if !out.contains(&id) {
        out.push(id);
    }
}

impl Query<'_> {
    /// In-class declarations of what the identifier at `offset` names (out-of-line
    /// definitions map back to the declaration they implement).
    pub fn declarations(&self, path: &str, text: &str, offset: u32) -> Vec<SymbolId> {
        let mut out = Vec::new();
        for id in self.definition(path, text, offset) {
            push_new(&mut out, self.idx.hier.canonical(id));
        }
        out
    }

    /// Overrides of a virtual method, the derived classes of a class, or the
    /// body of a non-virtual function.
    pub fn implementations(&self, path: &str, text: &str, offset: u32) -> Vec<SymbolId> {
        let (db, hier) = (&self.idx.db, &self.idx.hier);
        let mut out = Vec::new();
        for id in self.definition(path, text, offset) {
            let c = hier.canonical(id);
            let s = db.symbol(c);
            if s.kind.is_record() {
                let mut all = Vec::new();
                hier.all_derived(c, &mut all);
                for d in all {
                    if d != c && !db.is_dead(d) && db.symbol(d).kind.is_record() {
                        push_new(&mut out, d);
                    }
                }
            } else if s.kind.is_callable() {
                if s.flags.intersects(SymbolFlags::VIRTUAL | SymbolFlags::PURE | SymbolFlags::OVERRIDE) || s.family.is_some() {
                    let mut over = Vec::new();
                    hier.overriders(db, c, &mut over);
                    for x in over {
                        let m = hier.canonical(x);
                        let defs = hier.out_of_line_defs(m);
                        if defs.is_empty() {
                            push_new(&mut out, m);
                        }
                        for &d in defs {
                            push_new(&mut out, d);
                        }
                    }
                } else {
                    push_new(&mut out, id);
                }
            }
        }
        out
    }

    /// Classes behind the type of the variable, field, parameter, local or
    /// function result at `offset`; a type name names itself.
    pub fn type_definitions(&self, path: &str, text: &str, offset: u32) -> Vec<SymbolId> {
        let db = &self.idx.db;
        let mut out = Vec::new();
        let scopes = self.scopes_at(self.idx.file_index(path), offset);
        let cx = Cx { path, text, scopes: &scopes };
        let mut tys: Vec<Ty> = Vec::new();
        if let Some(l) = self.local_at(path, text, offset) {
            let written = if l.auto { self.local_type(path, text, &l.name, l.decl_offset) } else { Some(l.signature.clone()) };
            if let Some(w) = written {
                let toks = sig_tokens(&w);
                let end = if l.auto { toks.len() } else { toks.iter().rposition(|t| t.text(&w) == l.name).unwrap_or(toks.len()) };
                tys.extend(declared_type_tail(&toks[..end], &w));
            }
        } else {
            for id in self.definition(path, text, offset) {
                let s = db.symbol(id);
                if s.kind.is_type() {
                    let r = self.resolve_records([id]);
                    if r.is_empty() {
                        push_new(&mut out, id);
                    }
                    for c in r {
                        push_new(&mut out, c);
                    }
                } else if s.kind == SymbolKind::EnumConstant {
                    if let Some(p) = s.parent {
                        push_new(&mut out, p);
                    }
                } else {
                    tys.extend(self.decl_ty(&cx, id));
                }
            }
        }
        for t in tys {
            let found = db.lookup(&t.head).to_vec();
            let recs = self.resolve_records(found.iter().copied());
            if recs.is_empty() {
                for f in found.into_iter().filter(|&f| db.symbol(f).kind.is_type()) {
                    push_new(&mut out, f);
                }
            }
            for c in recs {
                push_new(&mut out, c);
            }
        }
        out
    }
}
