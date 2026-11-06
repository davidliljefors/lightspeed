//! `textDocument/prepareRename` and `textDocument/rename`.

use serde_json::{Value, json};

use crate::lsp::{Overlay, Server};
use crate::lsp::position::path_to_uri;
use crate::query::{Query, identifier_at};
use crate::base::source::LineIndex;

impl Server {
    /// Parameter/local at the request position: its occurrences as ranges in the same document.
    pub(in crate::lsp) fn local_ranges(&self, params: &Value) -> Option<(Value, Vec<Value>, String)> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs, self.index.as_ref());
        let l = Query::new(idx, &overlay).local_at(&path, text.as_str(), off)?;
        let now = self.now(&path)?;
        let uses = l.uses.iter().filter_map(|&(a, b)| now.range(a, b)).collect();
        Some((now.range(l.hovered.0, l.hovered.1)?, uses, l.name))
    }

    pub(in crate::lsp) fn inside_root(&self, path: &str) -> bool {
        let Some(root) = &self.root else { return true };
        let (p, r) = (path.replace('\\', "/").to_lowercase(), root.to_lowercase());
        p.strip_prefix(&r).is_some_and(|rest| rest.starts_with('/'))
    }

    pub(in crate::lsp) fn prepare_rename(&self, params: &Value) -> Option<Value> {
        if let Some((range, _, name)) = self.local_ranges(params) {
            return Some(json!({"range": range, "placeholder": name}));
        }
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs, self.index.as_ref());
        let q = Query::new(idx, &overlay);
        let seeds = q.definition(&path, text.as_str(), off);
        if seeds.is_empty() || seeds.iter().any(|&s| !self.inside_root(idx.db.files.path(idx.db.symbol(s).file))) {
            return None;
        }
        let (a, b) = identifier_at(text.as_str(), off)?;
        let range = Self::range_json(text.as_str(), &LineIndex::new(text.as_str()), (a, b));
        Some(json!({"range": range, "placeholder": &text.as_str()[a as usize..b as usize]}))
    }

    /// Workspace edit plus an optional warning for occurrences that were left alone.
    pub(in crate::lsp) fn rename(&self, params: &Value) -> Result<(Value, Option<String>), String> {
        let uri = params["textDocument"]["uri"].as_str().ok_or("missing document")?;
        let new_name = params["newName"].as_str().unwrap_or("");
        let valid = new_name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && new_name.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !valid {
            return Err(format!("`{new_name}` is not a valid identifier"));
        }
        if let Some((_, uses, _)) = self.local_ranges(params) {
            let edits: Vec<Value> = uses.into_iter().map(|r| json!({"range": r, "newText": new_name})).collect();
            return Ok((json!({"changes": {uri: edits}}), None));
        }
        let (path, text, off) = self.cursor(params).ok_or("cannot read document")?;
        let idx = self.index.as_ref().ok_or("not initialized")?;
        let overlay = Overlay(&self.docs, self.index.as_ref());
        let refs = Query::new(idx, &overlay).symbol_refs(&path, text.as_str(), off).ok_or("cannot resolve a symbol here")?;
        if let Some(&t) = refs.targets.iter().find(|&&t| !self.inside_root(idx.db.files.path(idx.db.symbol(t).file))) {
            return Err(format!("`{}` is declared outside the workspace ({}); not renaming", refs.name, idx.db.files.path(idx.db.symbol(t).file)));
        }
        let mut changes = serde_json::Map::new();
        let mut i = 0;
        while i < refs.certain.len() {
            let file = refs.certain[i].file;
            let j = i + refs.certain[i..].iter().take_while(|l| l.file == file).count();
            let fpath = &idx.files[file as usize].path;
            if self.inside_root(fpath)
                && let Some(t) = self.text_of(fpath)
            {
                let li = LineIndex::new(t.as_str());
                let edits: Vec<Value> = refs.certain[i..j]
                    .iter()
                    .map(|l| json!({"range": Self::range_json(t.as_str(), &li, (l.start, l.end)), "newText": new_name}))
                    .collect();
                changes.insert(path_to_uri(fpath), Value::Array(edits));
            }
            i = j;
        }
        let warning = (!refs.uncertain.is_empty()).then(|| {
            format!(
                "lightspeed: renamed {} occurrences of `{}`; skipped {} where the type could not be resolved (e.g. via `auto`/templates)",
                refs.certain.len(),
                refs.name,
                refs.uncertain.len()
            )
        });
        Ok((json!({"changes": changes}), warning))
    }
}
