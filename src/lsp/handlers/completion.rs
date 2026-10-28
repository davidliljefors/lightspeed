//! `textDocument/completion` and `textDocument/signatureHelp`.

use serde_json::{Value, json};

use crate::lsp::{Overlay, Server};
use hashbrown::HashSet;

use crate::lsp::convert::lsp_completion_kind;
use crate::query::Query;
use crate::symbols::SymbolId;

const COMPLETION_LIMIT: usize = 200;

impl Server {
    pub(in crate::lsp) fn signature_help(&self, params: &Value) -> Option<Value> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let h = Query::new(idx, &overlay).signature_help(&path, text.as_str(), off, self.settings.max_signatures)?;
        let more = (h.more > 0).then(|| json!({"kind": "markdown", "value": format!("… +{} more overload{}", h.more, if h.more == 1 { "" } else { "s" })}));
        let signatures: Vec<Value> = h
            .signatures
            .iter()
            .map(|s| {
                let mut v = json!({"label": s.label, "parameters": s.params.iter().map(|&(a, b)| json!({"label": [a, b]})).collect::<Vec<_>>()});
                if let Some(m) = &more {
                    v["documentation"] = m.clone();
                }
                v
            })
            .collect();
        Some(json!({"signatures": signatures, "activeSignature": 0, "activeParameter": h.active_param}))
    }

    pub(in crate::lsp) fn completion(&self, params: &Value) -> Option<Value> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let (syms, truncated) = Query::new(idx, &overlay).completions(&path, text.as_str(), off, COMPLETION_LIMIT);
        let mut seen: HashSet<(&str, u8)> = HashSet::new();
        let syms: Vec<SymbolId> = syms.into_iter().filter(|&s| seen.insert((idx.db.name(s), idx.db.symbol(s).kind as u8))).collect();
        let summaries = Query::new(idx, &overlay).summaries(&syms);
        let items: Vec<Value> = syms
            .iter()
            .zip(summaries)
            .map(|(&s, summary)| {
                let sym = idx.db.symbol(s);
                let container = sym.parent.map(|p| idx.db.qualified_name(p)).unwrap_or_default();
                json!({
                    "label": idx.db.name(s),
                    "kind": lsp_completion_kind(sym.kind),
                    "detail": summary.unwrap_or_else(|| container.clone()),
                    "labelDetails": {"description": container},
                })
            })
            .collect();
        Some(json!({"isIncomplete": truncated, "items": items}))
    }
}
