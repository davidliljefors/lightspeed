//! `lightspeed/findText`: text search over the indexed files' in-memory text.

use serde_json::{Value, json};

use crate::lsp::position::{offset_to_position, path_to_uri};
use crate::lsp::{Overlay, Server};
use crate::base::source::LineIndex;
use crate::query::TextSource;
use crate::query::textsearch::{self, Matcher, Options};

const DEFAULT_MAX: usize = 1000;
const PREVIEW_CHARS: usize = 240;

impl Server {
    pub(in crate::lsp) fn find_text(&self, params: &Value) -> Result<Option<Value>, String> {
        let query = params["query"].as_str().unwrap_or_default();
        let Some(idx) = self.index.as_ref().filter(|_| !query.is_empty()) else { return Ok(Some(json!({"matches": [], "truncated": false, "files": 0}))) };
        let flag = |k: &str, d: bool| params[k].as_bool().unwrap_or(d);
        let opts = Options { regex: flag("regex", false), case_sensitive: flag("caseSensitive", false), whole_word: flag("wholeWord", false) };
        let max = params["maxResults"].as_u64().map_or(DEFAULT_MAX, |n| (n as usize).clamp(1, 20_000));
        let matcher = Matcher::new(query, opts)?;

        let t0 = std::time::Instant::now();
        let overlay = Overlay(&self.docs, self.index.as_ref());
        let mut paths: Vec<&str> = idx.live_files().map(|(_, f)| f.path.as_str()).collect();
        paths.sort_unstable();
        let found = textsearch::search(&paths, &|p| overlay.text(p), &matcher, max);

        let mut matches = Vec::with_capacity(found.hits.len());
        let mut cur: Option<(usize, crate::query::Text, LineIndex, String)> = None;
        for h in &found.hits {
            if cur.as_ref().is_none_or(|c| c.0 != h.file) {
                cur = overlay.text(paths[h.file]).map(|t| {
                    let li = LineIndex::new(t.as_str());
                    (h.file, t, li, path_to_uri(paths[h.file]))
                });
            }
            let Some((_, text, li, uri)) = cur.as_ref() else { continue };
            let t = text.as_str();
            let (sl, sc) = offset_to_position(t, li, h.start);
            let (el, ec) = offset_to_position(t, li, h.end);
            matches.push(json!({
                "uri": uri,
                "range": {"start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec}},
                "preview": preview(t, li, sl as usize),
            }));
        }
        crate::log!("lightspeed: findText {query:?}: {} matches in {} files, {:.0} ms", matches.len(), found.files_searched, t0.elapsed().as_secs_f64() * 1e3);
        Ok(Some(json!({"matches": matches, "truncated": found.truncated, "files": found.files_searched})))
    }
}

/// The text of `line`, without indentation and cut to a sane length.
fn preview(text: &str, li: &LineIndex, line: usize) -> String {
    let start = li.line_start(line).unwrap_or(0) as usize;
    let rest = &text[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].trim().chars().take(PREVIEW_CHARS).collect()
}
