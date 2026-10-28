//! Index data -> LSP JSON: locations, ranges, symbol information and kind numbers.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::position::{offset_to_position, path_to_uri};
use super::{Overlay, Server};
use crate::query::{Location, Query, Text};
use crate::base::source::LineIndex;
use crate::symbols::{SymbolId, SymbolKind};

pub(super) fn lsp_symbol_kind(k: SymbolKind) -> u32 {
    match k {
        SymbolKind::Namespace => 3,
        SymbolKind::Class => 5,
        SymbolKind::Struct | SymbolKind::Union => 23,
        SymbolKind::Enum => 10,
        SymbolKind::EnumConstant => 22,
        SymbolKind::Function => 12,
        SymbolKind::Method => 6,
        SymbolKind::Constructor | SymbolKind::Destructor => 9,
        SymbolKind::Field => 8,
        SymbolKind::Variable => 13,
        SymbolKind::TypeAlias => 5,
        SymbolKind::Macro => 14,
    }
}

pub(super) fn lsp_completion_kind(k: SymbolKind) -> u32 {
    match k {
        SymbolKind::Namespace => 9,
        SymbolKind::Class | SymbolKind::TypeAlias => 7,
        SymbolKind::Struct | SymbolKind::Union => 22,
        SymbolKind::Enum => 13,
        SymbolKind::EnumConstant => 20,
        SymbolKind::Function => 3,
        SymbolKind::Method => 2,
        SymbolKind::Constructor | SymbolKind::Destructor => 4,
        SymbolKind::Field => 5,
        SymbolKind::Variable => 6,
        SymbolKind::Macro => 14,
    }
}

impl Server {
    pub(in crate::lsp) fn locations_json(&self, locs: &[Location]) -> Value {
        let idx = self.index.as_ref().unwrap();
        use rayon::prelude::*;
        // Group by file (first-seen order) so each file is read once, in parallel.
        let mut groups: Vec<(u32, Vec<usize>)> = Vec::new();
        let mut slot: HashMap<u32, usize> = HashMap::new();
        for (i, l) in locs.iter().enumerate() {
            let g = *slot.entry(l.file).or_insert_with(|| {
                groups.push((l.file, Vec::new()));
                groups.len() - 1
            });
            groups[g].1.push(i);
        }
        let done: Vec<Vec<(usize, Value)>> = groups
            .par_iter()
            .map(|(file, ids)| {
                let path = &idx.files[*file as usize].path;
                let Some(t) = self.text_of(path) else { return Vec::new() };
                let li = LineIndex::new(t.as_str());
                let uri = path_to_uri(path);
                ids.iter()
                    .map(|&i| {
                        let l = &locs[i];
                        let (s, e) = (offset_to_position(t.as_str(), &li, l.start), offset_to_position(t.as_str(), &li, l.end));
                        (i, json!({"uri": uri, "range": {"start": {"line": s.0, "character": s.1}, "end": {"line": e.0, "character": e.1}}}))
                    })
                    .collect()
            })
            .collect();
        let mut flat: Vec<(usize, Value)> = done.into_iter().flatten().collect();
        flat.sort_unstable_by_key(|p| p.0);
        Value::Array(flat.into_iter().map(|p| p.1).collect())
    }

    pub(in crate::lsp) fn range_json(text: &str, li: &LineIndex, (a, b): (u32, u32)) -> Value {
        let (s, e) = (offset_to_position(text, li, a), offset_to_position(text, li, b));
        json!({"start": {"line": s.0, "character": s.1}, "end": {"line": e.0, "character": e.1}})
    }

    /// `SymbolInformation` for each symbol; files are loaded and line-indexed once per call.
    pub(in crate::lsp) fn symbol_infos(&self, ids: impl IntoIterator<Item = SymbolId>) -> Value {
        let Some(idx) = self.index.as_ref() else { return Value::Array(Vec::new()) };
        let overlay = Overlay(&self.docs);
        let q = Query::new(idx, &overlay);
        let mut cache: HashMap<u32, Option<(Text, LineIndex, Value)>> = HashMap::new();
        let mut arr = Vec::new();
        for s in ids {
            let Some(loc) = q.location(s) else { continue };
            let entry = cache.entry(loc.file).or_insert_with(|| {
                let path = &idx.files[loc.file as usize].path;
                let t = self.text_of(path)?;
                let li = LineIndex::new(t.as_str());
                Some((t, li, Value::String(path_to_uri(path))))
            });
            let Some((t, li, uri)) = entry else { continue };
            let sym = idx.db.symbol(s);
            let container = sym.parent.map(|p| idx.db.qualified_name(p)).unwrap_or_default();
            arr.push(json!({
                "name": idx.db.name(s),
                "kind": lsp_symbol_kind(sym.kind),
                "containerName": container,
                "location": {"uri": uri, "range": Self::range_json(t.as_str(), li, (loc.start, loc.end))},
            }));
        }
        Value::Array(arr)
    }
}
