//! `textDocument/documentLink`: makes the header name of each resolved `#include` clickable.

use serde_json::{Value, json};

use crate::base::source::LineIndex;
use crate::lsp::Server;
use crate::lsp::position::{offset_to_position, path_to_uri, uri_to_path};

impl Server {
    pub(in crate::lsp) fn document_links(&self, params: &Value) -> Option<Value> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?)?;
        let text = self.text_of(&path)?;
        let text = text.as_str();
        let idx = self.index.as_ref()?;
        let li = LineIndex::new(text);
        let pos = |off| {
            let (line, character) = offset_to_position(text, &li, off);
            json!({"line": line, "character": character})
        };
        let links: Vec<Value> = idx
            .include_links(&path)
            .iter()
            .map(|l| json!({"range": {"start": pos(l.start), "end": pos(l.end)}, "target": path_to_uri(&l.path)}))
            .collect();
        Some(Value::Array(links))
    }
}
