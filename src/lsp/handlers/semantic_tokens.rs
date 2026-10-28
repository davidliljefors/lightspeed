//! `textDocument/semanticTokens/{full,full/delta,range}`.

use serde_json::{Value, json};

use crate::lsp::{Overlay, Server};
use crate::lsp::position::{offset_to_position, position_to_offset, uri_to_path};
use crate::pp::vfs::path_key;
use crate::query::Query;
use crate::base::source::LineIndex;

impl Server {
    /// Encoded token data for the document (or the requested range of it).
    pub(in crate::lsp) fn semantic_data(&self, params: &Value, ranged: bool) -> Option<(String, Vec<u32>)> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?)?;
        let text = self.text_of(&path)?;
        let text = text.as_str();
        let idx = self.index.as_ref()?;
        let li = LineIndex::new(text);
        let range = ranged.then(|| {
            let at = |p: &Value| position_to_offset(text, &li, p["line"].as_u64().unwrap_or(0) as u32, p["character"].as_u64().unwrap_or(0) as u32);
            (at(&params["range"]["start"]), at(&params["range"]["end"]))
        });
        let overlay = Overlay(&self.docs);
        let toks = Query::new(idx, &overlay).semantic_tokens(&path, text, range);
        let mut data = Vec::with_capacity(toks.len() * 5);
        let (mut pl, mut pc) = (0u32, 0u32);
        for t in toks {
            let (line, col) = offset_to_position(text, &li, t.start);
            let end = offset_to_position(text, &li, t.start + t.len);
            data.extend([line - pl, if line == pl { col - pc } else { col }, end.1.saturating_sub(col), t.ty as u32, t.mods as u32]);
            (pl, pc) = (line, col);
        }
        Some((path, data))
    }

    pub(in crate::lsp) fn semantic_tokens(&self, params: &Value, ranged: bool) -> Option<Value> {
        let (path, data) = self.semantic_data(params, ranged)?;
        if ranged {
            return Some(json!({"data": data}));
        }
        let id = self.sem.next_id();
        let reply = json!({"resultId": id.to_string(), "data": data});
        self.sem.docs.lock().unwrap().insert(path_key(&path), (id, data));
        Some(reply)
    }

    /// Answers with the single splice that turns the previous token array into the new one,
    /// or with the full data when the client's `previousResultId` is not the one we last sent.
    pub(in crate::lsp) fn semantic_tokens_delta(&self, params: &Value) -> Option<Value> {
        let (path, data) = self.semantic_data(params, false)?;
        let key = path_key(&path);
        let id = self.sem.next_id();
        let prev = params["previousResultId"].as_str().and_then(|s| s.parse::<u64>().ok());
        let mut cache = self.sem.docs.lock().unwrap();
        let reply = match cache.get(&key).filter(|(old_id, _)| Some(*old_id) == prev) {
            Some((_, old)) => {
                let prefix = old.iter().zip(&data).take_while(|(a, b)| a == b).count();
                let suffix = old[prefix..].iter().rev().zip(data[prefix..].iter().rev()).take_while(|(a, b)| a == b).count();
                let edits = if prefix == old.len() && prefix == data.len() {
                    json!([])
                } else {
                    json!([{"start": prefix, "deleteCount": old.len() - prefix - suffix, "data": &data[prefix..data.len() - suffix]}])
                };
                json!({"resultId": id.to_string(), "edits": edits})
            }
            None => json!({"resultId": id.to_string(), "data": data}),
        };
        cache.insert(key, (id, data));
        Some(reply)
    }
}
