//! `textDocument/hover`.

use serde_json::{Value, json};

use crate::lsp::{Overlay, Server};
use crate::query::Query;

impl Server {
    pub(in crate::lsp) fn hover(&self, params: &Value) -> Option<Value> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let h = Query::new(idx, &overlay).hover(&path, text.as_str(), off)?;
        let mut md = format!("```cpp\n{}\n```\n", h.signature);
        let mut facts = h.kind.clone();
        if !h.container.is_empty() {
            facts = format!("{facts} in `{}`", h.container);
        }
        for n in &h.notes {
            facts.push_str(" · ");
            facts.push_str(n);
        }
        md.push_str(&facts);
        if let Some(doc) = &h.doc {
            md.push_str("\n\n---\n");
            md.push_str(doc);
        }
        if let Some(now) = self.now(&h.path)
            && let Some(line) = now.line(h.offset)
        {
            let file = h.path.rsplit('/').next().unwrap_or(&h.path);
            md.push_str(&format!("\n\n---\n`{file}:{}`", line + 1));
            if h.more > 0 {
                md.push_str(&format!(" (+{} other candidate{})", h.more, if h.more == 1 { "" } else { "s" }));
            }
        }
        Some(json!({
            "contents": {"kind": "markdown", "value": md},
            "range": self.now(&path)?.range(h.start, h.end)?,
        }))
    }
}
