//! `textDocument/documentSymbol` and `workspace/symbol`.

use serde_json::Value;

use crate::lsp::{Overlay, Server};
use crate::lsp::position::uri_to_path;
use crate::query::Query;

const WORKSPACE_SYMBOL_LIMIT: usize = 200;

impl Server {
    pub(in crate::lsp) fn document_symbols(&self, params: &Value) -> Option<Value> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?)?;
        let idx = self.index.as_ref()?;
        let fi = idx.file_index(&path)?;
        Some(self.symbol_infos(idx.file_symbols(fi)))
    }

    pub(in crate::lsp) fn workspace_symbols(&self, params: &Value) -> Option<Value> {
        let q = params["query"].as_str()?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let hits = Query::new(idx, &overlay).search_symbols(q, WORKSPACE_SYMBOL_LIMIT);
        Some(self.symbol_infos(hits))
    }
}
