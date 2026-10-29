//! definition, declaration, implementation, typeDefinition and references.

use serde_json::{Value, json};

use crate::lsp::{Overlay, Server};
use crate::query::{Location, Query, identifier_at};
use crate::base::source::LineIndex;
use crate::symbols::SymbolId;

impl Server {
    pub(in crate::lsp) fn definition(&self, params: &Value) -> Option<Value> {
        self.navigate(params, true, true, |q, p, t, o| q.definition(p, t, o))
    }

    /// Shared body of the "go to ..." requests. Locals are not indexed symbols,
    /// so when `locals_here` the declaration of a local is the answer. With
    /// `stay_put`, a cursor already on the only answer gets no answer at all:
    /// editors react to "you are here" by launching a find-references search.
    pub(in crate::lsp) fn navigate(&self, params: &Value, locals_here: bool, stay_put: bool, find: impl Fn(&Query, &str, &str, u32) -> Vec<SymbolId>) -> Option<Value> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let q = Query::new(idx, &overlay);
        if locals_here && let Some(l) = q.local_at(&path, text.as_str(), off) {
            if stay_put && l.decl.0 <= off && off <= l.decl.1 {
                return Some(json!([]));
            }
            let uri = params["textDocument"]["uri"].as_str()?;
            let range = Self::range_json(text.as_str(), &LineIndex::new(text.as_str()), l.decl);
            return Some(json!([{"uri": uri, "range": range}]));
        }
        let locs: Vec<Location> = find(&q, &path, text.as_str(), off).into_iter().filter_map(|s| q.location(s)).collect();
        if stay_put && let [only] = locs.as_slice() && idx.file_index(&path) == Some(only.file as usize) && only.start <= off && off <= only.end {
            return Some(json!([]));
        }
        Some(self.locations_json(&locs))
    }

    pub(in crate::lsp) fn references(&self, params: &Value) -> Option<Value> {
        if let Some((_, uses, _)) = self.local_ranges(params) {
            let uri = params["textDocument"]["uri"].as_str()?;
            return Some(Value::Array(uses.into_iter().map(|r| json!({"uri": uri, "range": r})).collect()));
        }
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let q = Query::new(idx, &overlay);
        if let Some(p) = self.partial.as_deref() {
            // Each file's hits go out as soon as it is scanned; `finish` sends the remainder.
            let certain_only = self.settings.certain_only;
            let found = q.symbol_refs_streaming(&path, text.as_str(), off, &|c, u| {
                let mut locs = c.to_vec();
                if !certain_only {
                    locs.extend_from_slice(u);
                }
                if let Value::Array(items) = self.locations_json(&locs) {
                    p.push(items);
                }
            });
            if found.is_some() {
                return Some(json!([]));
            }
        } else if let Some(r) = q.symbol_refs(&path, text.as_str(), off) {
            let mut locs = r.certain;
            if !self.settings.certain_only {
                locs.extend(r.uncertain);
                locs.sort_unstable();
            }
            return Some(self.locations_json(&locs));
        }
        if self.settings.certain_only {
            return Some(json!([]));
        }
        let (a, b) = identifier_at(text.as_str(), off)?;
        let name = &text.as_str()[a as usize..b as usize];
        let locs = q.references(name);
        Some(self.locations_json(&locs))
    }
}
