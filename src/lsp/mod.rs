//! Minimal Language Server: JSON-RPC over stdio with `Content-Length` framing.
//!
//! [`Server::handle`] is a pure message -> responses function, so the whole
//! protocol can be driven in-process by tests; [`Server::run`] wires it to
//! stdio, a file watcher and an idle timer.
//!
//! Edits are applied lazily: `didChange` only records the new buffer, and the
//! index is patched (one file, then an O(symbols) flush) right before the next
//! request or when the client goes idle. Position-based "go to" requests don't
//! wait for that: they are answered from the index as it is and mapped through
//! the edit (see `stale`).
//!
//! Layout: `transport` (framing), `position` (URIs, UTF-16), `session` (the loop,
//! background indexing, watcher), `router` (method table), `handlers/` (one file per
//! feature), `convert` (index data -> LSP JSON), `stale` (buffers ahead of the index).

mod convert;
mod handlers;
mod position;
mod router;
mod session;
mod stale;
mod stream;
#[cfg(test)]
mod tests;
mod transport;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use hashbrown::HashSet;
use serde_json::Value;

use crate::workspace::index::{Index, Progress};
use crate::pp::vfs::path_key;
use crate::query::{DiskText, Text, TextSource, identifier_at};
use crate::base::source::LineIndex;

pub use position::{offset_to_position, path_to_uri, position_to_offset, uri_to_path};
pub use transport::{read_frame, serve_stdio, write_frame};

const PROGRESS_TOKEN: &str = "lightspeed/index";

pub enum Msg {
    Rpc(Value),
    Fs(PathBuf),
    /// Background build finished: the index and how long it took (ms).
    Indexed(Box<Index>, f64),
    Eof,
}

struct Doc {
    path: String,
    /// What the editor holds.
    text: Arc<str>,
    /// The text the index was last built from, once known. Differs from `text` while an edit is pending.
    indexed: Option<Arc<str>>,
    /// Buffer differs from what the index holds.
    dirty: bool,
}

impl Doc {
    fn new(path: String, text: &str, indexed: Option<Arc<str>>) -> Self {
        Self { path, text: text.into(), indexed, dirty: true }
    }

    /// The text the index (and so every query) sees.
    fn seen(&self) -> &Arc<str> {
        self.indexed.as_ref().unwrap_or(&self.text)
    }

    /// The index lags behind the buffer.
    fn ahead(&self) -> bool {
        self.indexed.as_ref().is_some_and(|i| !Arc::ptr_eq(i, &self.text))
    }
}

/// Open buffers shadow the disk, as the index last saw them.
struct Overlay<'a>(&'a HashMap<u64, Doc>, Option<&'a Index>);

impl TextSource for Overlay<'_> {
    fn text(&self, path: &str) -> Option<Text> {
        match self.0.get(&path_key(path)) {
            Some(d) => Some(Text::Shared(d.seen().clone())),
            None => match self.1 {
                Some(idx) => idx.texts.text(path),
                None => DiskText.text(path),
            },
        }
    }
}

/// A file as the editor shows it now, plus how to read positions the index gives for it.
pub(super) struct Now {
    cur: Text,
    li: LineIndex,
    /// Set while the index lags: the indexed text and its relation to `cur`.
    from: Option<(Text, stale::EditMap)>,
}

impl Now {
    /// A range of the indexed text as an LSP range in the current text; `None` when the edit touches it.
    pub(super) fn range(&self, a: u32, b: u32) -> Option<Value> {
        let (a, b) = match &self.from {
            Some((old, em)) => em.old_to_new(old.as_str(), self.cur.as_str(), a, b)?,
            None => (a, b),
        };
        let t = self.cur.as_str();
        let (s, e) = (offset_to_position(t, &self.li, a), offset_to_position(t, &self.li, b));
        Some(serde_json::json!({"start": {"line": s.0, "character": s.1}, "end": {"line": e.0, "character": e.1}}))
    }

    /// The line (0-based) of an offset of the indexed text.
    pub(super) fn line(&self, off: u32) -> Option<u32> {
        self.range(off, off).and_then(|r| r["start"]["line"].as_u64()).map(|l| l as u32)
    }
}

/// What the client told us it can do in `initialize`.
#[derive(Default)]
struct ClientCaps {
    /// `window/workDoneProgress/create`.
    progress: bool,
    /// `workspace/semanticTokens/refresh`.
    token_refresh: bool,
}

/// User settings (`initializationOptions` / `didChangeConfiguration`).
struct Settings {
    /// How many overloads signature help lists (`signatureHelp.maxOverloads`).
    max_signatures: usize,
    /// Find-references lists only occurrences proven to be the symbol (`references.certainOnly`);
    /// otherwise unresolved same-named occurrences are included too. Rename is always certain-only.
    certain_only: bool,
    /// Send long result lists as `$/progress` batches when the client asks for them (`streaming`).
    stream: bool,
}

/// Last full token data sent per open document, for `full/delta`. Behind a mutex
/// because handlers run on `&Server` (and fan out across threads).
#[derive(Default)]
struct SemanticCache {
    docs: std::sync::Mutex<HashMap<u64, (u64, Vec<u32>)>>,
    seq: std::sync::atomic::AtomicU64,
}

impl SemanticCache {
    fn next_id(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }
}

pub struct Server {
    cdb_hint: Option<PathBuf>,
    /// A generated `.ninja` file used instead of a compile database; no workspace search.
    ninja: Option<PathBuf>,
    index: Option<Index>,
    docs: HashMap<u64, Doc>,
    fs_dirty: HashSet<String>,
    events: Option<Sender<Msg>>,
    _watcher: Option<notify::RecommendedWatcher>,
    /// Workspace root (forward slashes); rename refuses to touch files outside it.
    root: Option<String>,
    /// Build the index on a background thread (set by [`Self::run`]); requests answer
    /// empty until it lands. Tests drive `handle` directly and index synchronously.
    background: bool,
    building: Option<Arc<Progress>>,
    progress_started: bool,
    last_progress: ((u32, u32), std::time::Instant),
    shutdown: bool,
    caps: ClientCaps,
    settings: Settings,
    refresh_seq: u64,
    sem: SemanticCache,
    /// Where streamed batches go the moment they are ready; without one they are queued for handle's output.
    sink: Option<stream::Sink>,
    /// The running request's partial-result collector, if it has one.
    partial: Option<Arc<stream::Partial>>,
    pub exit_code: Option<i32>,
}

impl Server {
    pub fn new(cdb_hint: Option<PathBuf>) -> Self {
        Self {
            cdb_hint,
            ninja: None,
            index: None,
            docs: HashMap::new(),
            fs_dirty: HashSet::new(),
            events: None,
            _watcher: None,
            root: None,
            background: false,
            building: None,
            progress_started: false,
            last_progress: ((0, 0), std::time::Instant::now()),
            shutdown: false,
            caps: ClientCaps::default(),
            settings: Settings { max_signatures: 3, certain_only: false, stream: true },
            refresh_seq: 0,
            sem: SemanticCache::default(),
            sink: None,
            partial: None,
            exit_code: None,
        }
    }

    /// Reads `{"signatureHelp": {"maxOverloads": n}, "references": {"certainOnly": bool}}` from `initializationOptions`
    /// or a `workspace/didChangeConfiguration` payload (optionally under `lightspeed`).
    pub(in crate::lsp) fn apply_settings(&mut self, v: &Value) {
        let v = v.get("lightspeed").unwrap_or(v);
        if let Some(n) = v.pointer("/signatureHelp/maxOverloads").and_then(Value::as_u64) {
            self.settings.max_signatures = (n as usize).clamp(1, 20);
        }
        if let Some(b) = v.pointer("/references/certainOnly").and_then(Value::as_bool) {
            self.settings.certain_only = b;
        }
        if let Some(b) = v.get("streaming").and_then(Value::as_bool) {
            self.settings.stream = b;
        }
    }

    /// Starting value of `references.certainOnly`; client settings still override it.
    pub fn set_certain_only(&mut self, on: bool) {
        self.settings.certain_only = on;
    }

    /// Starting value of `streaming`; client settings still override it.
    pub fn set_streaming(&mut self, on: bool) {
        self.settings.stream = on;
    }

    /// Index from a generated `.ninja` file instead of a `compile_commands.json`.
    pub fn set_ninja(&mut self, path: Option<PathBuf>) {
        self.ninja = path;
    }

    /// Sends streamed batches through send as they are ready, from whichever thread produced them.
    pub fn set_sink(&mut self, send: impl Fn(Value) + Send + Sync + 'static) {
        self.sink = Some(Arc::new(send));
    }

    pub fn index(&self) -> Option<&Index> {
        self.index.as_ref()
    }

    pub(in crate::lsp) fn text_of(&self, path: &str) -> Option<Text> {
        Overlay(&self.docs, self.index.as_ref()).text(path)
    }

    /// `path` as the editor shows it now.
    pub(in crate::lsp) fn now(&self, path: &str) -> Option<Now> {
        let Some(d) = self.docs.get(&path_key(path)) else {
            let cur = DiskText.text(path)?;
            return Some(Now { li: LineIndex::new(cur.as_str()), cur, from: None });
        };
        let cur = Text::Shared(d.text.clone());
        let from = d.ahead().then(|| {
            let old = Text::Shared(d.seen().clone());
            let em = stale::EditMap::between(old.as_str(), cur.as_str());
            (old, em)
        });
        Some(Now { li: LineIndex::new(cur.as_str()), cur, from })
    }

    /// Document path, the text the index has for it, and the byte offset of the request position
    /// in that text. When the buffer is ahead of the index the position is mapped back through the
    /// edit; `None` if the edit covers what the cursor is on.
    pub(in crate::lsp) fn cursor(&self, params: &Value) -> Option<(String, Text, u32)> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?)?;
        let (line, col) = (params["position"]["line"].as_u64()? as u32, params["position"]["character"].as_u64()? as u32);
        let Some(d) = self.docs.get(&path_key(&path)).filter(|d| d.ahead()) else {
            let text = self.text_of(&path)?;
            let off = position_to_offset(text.as_str(), &LineIndex::new(text.as_str()), line, col);
            return Some((path, text, off));
        };
        let (cur, old) = (&*d.text, d.seen());
        let off = position_to_offset(cur, &LineIndex::new(cur), line, col);
        let (a, b) = identifier_at(cur, off).unwrap_or((off, off));
        let (old_a, _) = stale::EditMap::between(old, cur).new_to_old(old, cur, a, b)?;
        Some((path, Text::Shared(old.clone()), old_a + (off - a)))
    }

    /// Whether `method` can be answered without first indexing the buffer's pending edit.
    pub(in crate::lsp) fn can_defer(&self, method: &str, params: &Value) -> bool {
        const DEFERRABLE: &[&str] = &[
            "textDocument/definition",
            "textDocument/declaration",
            "textDocument/implementation",
            "textDocument/typeDefinition",
            "textDocument/references",
            "textDocument/hover",
        ];
        if !DEFERRABLE.contains(&method) {
            return false;
        }
        let Some(path) = params["textDocument"]["uri"].as_str().and_then(uri_to_path) else { return false };
        let Some(d) = self.docs.get(&path_key(&path)).filter(|d| d.dirty) else { return true };
        let (Some(line), Some(col)) = (params["position"]["line"].as_u64(), params["position"]["character"].as_u64()) else { return false };
        if !d.ahead() {
            return false;
        }
        let (cur, old) = (&*d.text, d.seen());
        let off = position_to_offset(cur, &LineIndex::new(cur), line as u32, col as u32);
        let em = stale::EditMap::between(old, cur);
        let (a, b) = identifier_at(cur, off).unwrap_or((off, off));
        em.new_to_old(old, cur, a, b).is_some() && !em.touches_line(cur, off)
    }
}
