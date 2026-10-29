//! Minimal Language Server: JSON-RPC over stdio with `Content-Length` framing.
//!
//! [`Server::handle`] is a pure message -> responses function, so the whole
//! protocol can be driven in-process by tests; [`Server::run`] wires it to
//! stdio, a file watcher and an idle timer.
//!
//! Edits are applied lazily: `didChange` only records the new buffer, and the
//! index is patched (one file, then an O(symbols) flush) right before the next
//! request or when the client goes idle.
//!
//! Layout: `transport` (framing), `position` (URIs, UTF-16), `session` (the loop,
//! background indexing, watcher), `router` (method table), `handlers/` (one file per
//! feature), `convert` (index data -> LSP JSON).

mod convert;
mod handlers;
mod position;
mod router;
mod session;
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
use crate::query::{DiskText, Text, TextSource};
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
    text: Arc<str>,
    /// Buffer differs from what the index holds.
    dirty: bool,
}

/// Open buffers shadow the disk.
struct Overlay<'a>(&'a HashMap<u64, Doc>);

impl TextSource for Overlay<'_> {
    fn text(&self, path: &str) -> Option<Text> {
        match self.0.get(&path_key(path)) {
            Some(d) => Some(Text::Shared(d.text.clone())),
            None => DiskText.text(path),
        }
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
    pub exit_code: Option<i32>,
}

impl Server {
    pub fn new(cdb_hint: Option<PathBuf>) -> Self {
        Self {
            cdb_hint,
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
            settings: Settings { max_signatures: 3, certain_only: false },
            refresh_seq: 0,
            sem: SemanticCache::default(),
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
    }

    /// Starting value of `references.certainOnly`; client settings still override it.
    pub fn set_certain_only(&mut self, on: bool) {
        self.settings.certain_only = on;
    }

    pub fn index(&self) -> Option<&Index> {
        self.index.as_ref()
    }

    pub(in crate::lsp) fn text_of(&self, path: &str) -> Option<Text> {
        Overlay(&self.docs).text(path)
    }

    /// Document path, its text and the byte offset of the request position.
    pub(in crate::lsp) fn cursor(&self, params: &Value) -> Option<(String, Text, u32)> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?)?;
        let text = self.text_of(&path)?;
        let (line, col) = (params["position"]["line"].as_u64()? as u32, params["position"]["character"].as_u64()? as u32);
        let li = LineIndex::new(text.as_str());
        let off = position_to_offset(text.as_str(), &li, line, col);
        Some((path, text, off))
    }
}
