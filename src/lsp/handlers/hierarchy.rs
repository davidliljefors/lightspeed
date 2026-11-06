//! Type hierarchy and the custom `lightspeed/inheritanceGraph` request.

use serde_json::{Value, json};

use crate::lsp::{Overlay, Server};
use std::collections::HashMap;

use hashbrown::HashSet;

use crate::base::ids::Idx;
use crate::lsp::convert::lsp_symbol_kind;
use crate::lsp::position::{path_to_uri, position_to_offset, uri_to_path};
use crate::query::{Query, Text};
use crate::base::source::LineIndex;
use crate::symbols::SymbolId;

impl Server {
    /// `TypeHierarchyItem`s (also the nodes of the inheritance graph) for `ids`.
    pub(in crate::lsp) fn type_items(&self, ids: &[SymbolId]) -> Vec<Value> {
        let Some(idx) = self.index.as_ref() else { return Vec::new() };
        let overlay = Overlay(&self.docs, self.index.as_ref());
        let q = Query::new(idx, &overlay);
        let mut cache: HashMap<u32, Option<(Text, LineIndex, Value)>> = HashMap::new();
        let mut out = Vec::with_capacity(ids.len());
        for &s in ids {
            let Some(loc) = q.location(s) else { continue };
            let entry = cache.entry(loc.file).or_insert_with(|| {
                let path = &idx.files[loc.file as usize].path;
                let t = self.text_of(path)?;
                let li = LineIndex::new(t.as_str());
                Some((t, li, Value::String(path_to_uri(path))))
            });
            let Some((t, li, uri)) = entry else { continue };
            let sym = idx.db.symbol(s);
            let range = Self::range_json(t.as_str(), li, (loc.start, loc.end));
            out.push(json!({
                "name": idx.db.name(s),
                "kind": lsp_symbol_kind(sym.kind),
                "detail": idx.db.qualified_name(s),
                "uri": uri,
                "range": range,
                "selectionRange": range,
                "data": {"id": s.index()},
            }));
        }
        out
    }

    /// The class a hierarchy request refers to: a `data.id` item, a `typeName`
    /// (qualified or not), or the type at `textDocument` + `position`.
    pub(in crate::lsp) fn hierarchy_class(&self, params: &Value) -> Option<SymbolId> {
        let idx = self.index.as_ref()?;
        let db = &idx.db;
        let item = params.get("item").unwrap_or(params);
        let overlay = Overlay(&self.docs, self.index.as_ref());
        // Items are resolved by where the name is written now: symbol ids are
        // renumbered when the index is rebuilt after an edit, positions are not.
        if let (Some(uri), Some(line), Some(col)) = (item["uri"].as_str(), item["selectionRange"]["start"]["line"].as_u64(), item["selectionRange"]["start"]["character"].as_u64())
            && let Some(path) = uri_to_path(uri)
            && let Some(text) = self.text_of(&path)
        {
            let off = position_to_offset(text.as_str(), &LineIndex::new(text.as_str()), line as u32, col as u32);
            if let Some(id) = Query::new(idx, &overlay).class_at(&path, text.as_str(), off) {
                return Some(id);
            }
        }
        if let Some(raw) = item["data"]["id"].as_u64().or_else(|| params["id"].as_u64()) {
            let raw = raw as usize;
            return (raw < db.len() && db.symbol(SymbolId::from_usize(raw)).kind.is_record()).then(|| SymbolId::from_usize(raw));
        }
        if let Some(name) = params["typeName"].as_str() {
            let last = name.rsplit("::").next()?;
            return db.lookup(last).iter().copied().find(|&c| db.symbol(c).kind.is_record() && db.qualified_name(c).ends_with(name));
        }
        let (path, text, off) = self.cursor(params)?;
        Query::new(idx, &overlay).class_at(&path, text.as_str(), off)
    }

    pub(in crate::lsp) fn prepare_type_hierarchy(&self, params: &Value) -> Option<Value> {
        let id = self.hierarchy_class(params)?;
        Some(Value::Array(self.type_items(&[id])))
    }

    pub(in crate::lsp) fn type_hierarchy_step(&self, params: &Value, down: bool) -> Option<Value> {
        let id = self.hierarchy_class(params)?;
        let hier = &self.index.as_ref()?.hier;
        let ids: Vec<SymbolId> = if down { hier.derived(id) } else { hier.bases(id) }.to_vec();
        Some(Value::Array(self.type_items(&ids)))
    }

    /// Custom request `lightspeed/inheritanceGraph`: the class's ancestors and
    /// descendants as nodes and `{derived, base}` edges, for a graph view.
    /// Optional `maxDepth` (default 8) and `maxNodes` (default 300) bound the walk.
    pub(in crate::lsp) fn inheritance_graph(&self, params: &Value) -> Option<Value> {
        let root = self.hierarchy_class(params)?;
        let hier = &self.index.as_ref()?.hier;
        let max_depth = params["maxDepth"].as_u64().unwrap_or(8) as usize;
        let max_nodes = params["maxNodes"].as_u64().unwrap_or(300) as usize;
        let mut nodes = vec![root];
        let mut seen: HashSet<SymbolId> = HashSet::new();
        seen.insert(root);
        let mut edges: Vec<(SymbolId, SymbolId)> = Vec::new();
        let mut truncated = false;
        for down in [false, true] {
            let mut level = vec![root];
            for _ in 0..max_depth {
                let mut next = Vec::new();
                for &c in &level {
                    for &n in if down { hier.derived(c) } else { hier.bases(c) } {
                        let (derived, base) = if down { (n, c) } else { (c, n) };
                        if seen.insert(n) {
                            if nodes.len() >= max_nodes {
                                truncated = true;
                                seen.remove(&n);
                                continue;
                            }
                            nodes.push(n);
                            next.push(n);
                        }
                        if seen.contains(&n) {
                            edges.push((derived, base));
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                level = next;
            }
        }
        let items = self.type_items(&nodes);
        let edges: Vec<Value> = edges.iter().map(|&(d, b)| json!({"derived": d.index(), "base": b.index()})).collect();
        Some(json!({"root": root.index(), "nodes": items, "edges": edges, "truncated": truncated}))
    }
}
