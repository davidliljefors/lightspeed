//! Minimal Language Server: JSON-RPC over stdio with `Content-Length` framing.
//!
//! [`Server::handle`] is a pure message -> responses function, so the whole
//! protocol can be driven in-process by tests; [`Server::run`] wires it to
//! stdio, a file watcher and an idle timer.
//!
//! Edits are applied lazily: `didChange` only records the new buffer, and the
//! index is patched (one file, then an O(symbols) flush) right before the next
//! request or when the client goes idle.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use hashbrown::HashSet;
use serde_json::{Value, json};

use crate::compile_commands::CompileDb;
use crate::hash::hash_bytes;
use crate::ids::Idx;
use crate::index::{Index, IndexConfig, Progress};
use crate::pp::vfs::{normalize_path, path_key};
use crate::query::{DiskText, Location, Query, Text, TextSource, identifier_at};
use crate::source::LineIndex;
use crate::symbols::{SymbolId, SymbolKind};

const IDLE_FLUSH: Duration = Duration::from_millis(150);
const COMPLETION_LIMIT: usize = 200;
const WORKSPACE_SYMBOL_LIMIT: usize = 200;

pub enum Msg {
    Rpc(Value),
    Fs(PathBuf),
    /// Background build finished: the index and how long it took (ms).
    Indexed(Box<Index>, f64),
    Eof,
}

const PROGRESS_TOKEN: &str = "lightspeed/index";

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
    /// The client accepts `window/workDoneProgress/create`.
    progress_ok: bool,
    progress_started: bool,
    last_progress: ((u32, u32), std::time::Instant),
    shutdown: bool,
    /// How many overloads signature help lists (`signatureHelp.maxOverloads`).
    max_signatures: usize,
    pub exit_code: Option<i32>,
}

// ---- URIs and positions -------------------------------------------------

pub fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let r = rest.as_bytes();
    let mut i = 0;
    while i < r.len() {
        if r[i] == b'%' && i + 2 < r.len() && let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
            bytes.push(v);
            i += 3;
        } else {
            bytes.push(r[i]);
            i += 1;
        }
    }
    let mut p = String::from_utf8(bytes).ok()?;
    let b = p.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        p.remove(0);
    }
    normalize_path(&mut p);
    Some(p)
}

pub fn path_to_uri(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 12);
    out.push_str("file://");
    if !path.starts_with('/') {
        out.push('/');
    }
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => out.push(b as char),
            b'\\' => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Byte offset -> LSP (line, UTF-16 column).
pub fn offset_to_position(text: &str, li: &LineIndex, mut off: u32) -> (u32, u32) {
    off = off.min(text.len() as u32);
    while !text.is_char_boundary(off as usize) {
        off -= 1;
    }
    let line = li.line_of(off);
    let start = li.line_start(line).unwrap_or(0) as usize;
    let col: usize = text[start..off as usize].chars().map(char::len_utf16).sum();
    (line as u32, col as u32)
}

/// LSP (line, UTF-16 column) -> byte offset, clamped to the line's end.
pub fn position_to_offset(text: &str, li: &LineIndex, line: u32, col: u32) -> u32 {
    let Some(start) = li.line_start(line as usize) else { return text.len() as u32 };
    let mut units = 0u32;
    for (i, c) in text[start as usize..].char_indices() {
        if units >= col || c == '\n' {
            return start + i as u32;
        }
        units += c.len_utf16() as u32;
    }
    text.len() as u32
}

fn lsp_symbol_kind(k: SymbolKind) -> u32 {
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

fn lsp_completion_kind(k: SymbolKind) -> u32 {
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

// ---- framing --------------------------------------------------------------

/// Reads one `Content-Length`-framed message. `Ok(None)` on clean EOF.
pub fn read_frame(r: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut len: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let l = line.trim_end();
        if l.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some((k, v)) = l.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().ok();
        }
    }
    let mut buf = vec![0; len.unwrap_or(0)];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn write_frame(w: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

fn is_source_ext(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(e.to_ascii_lowercase().as_str(), "c" | "cc" | "cpp" | "cxx" | "c++" | "h" | "hh" | "hpp" | "hxx" | "inl" | "ipp" | "tpp")
    })
}

fn respond(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
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
            progress_ok: false,
            progress_started: false,
            last_progress: ((0, 0), std::time::Instant::now()),
            shutdown: false,
            max_signatures: 3,
            exit_code: None,
        }
    }

    /// Reads `{"signatureHelp": {"maxOverloads": n}}` from `initializationOptions`
    /// or a `workspace/didChangeConfiguration` payload (optionally under `lightspeed`).
    fn apply_settings(&mut self, v: &Value) {
        let v = v.get("lightspeed").unwrap_or(v);
        if let Some(n) = v.pointer("/signatureHelp/maxOverloads").and_then(Value::as_u64) {
            self.max_signatures = (n as usize).clamp(1, 20);
        }
    }

    pub fn index(&self) -> Option<&Index> {
        self.index.as_ref()
    }

    /// Serves until the client exits or closes stdin. Returns the exit code.
    pub fn run(mut self, input: impl BufRead + Send + 'static, mut output: impl Write) -> i32 {
        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
        self.events = Some(tx.clone());
        self.background = true;
        let reader_tx = tx;
        std::thread::spawn(move || {
            let mut input = input;
            loop {
                match read_frame(&mut input) {
                    Ok(Some(buf)) => match serde_json::from_slice(&buf) {
                        Ok(v) => {
                            if reader_tx.send(Msg::Rpc(v)).is_err() {
                                return;
                            }
                        }
                        Err(e) => eprintln!("lightspeed: bad JSON: {e}"),
                    },
                    _ => {
                        let _ = reader_tx.send(Msg::Eof);
                        return;
                    }
                }
            }
        });

        let mut out = Vec::new();
        // Messages already read from the client, so stale requests can be cancelled before they run.
        let mut queue: std::collections::VecDeque<Msg> = std::collections::VecDeque::new();
        let mut cancelled: Vec<Value> = Vec::new();
        let note = |m: &Msg, cancelled: &mut Vec<Value>| {
            if let Msg::Rpc(v) = m
                && v["method"] == "$/cancelRequest"
            {
                cancelled.push(v["params"]["id"].clone());
                if cancelled.len() > 256 {
                    cancelled.remove(0);
                }
            }
        };
        loop {
            self.tick_progress(&mut out);
            let wait = if self.building.is_some() { Duration::from_millis(100) } else { IDLE_FLUSH };
            let next = match queue.pop_front() {
                Some(m) => Ok(m),
                None => rx.recv_timeout(wait).inspect(|m| note(m, &mut cancelled)),
            };
            if next.is_ok() {
                for m in rx.try_iter() {
                    note(&m, &mut cancelled);
                    queue.push_back(m);
                }
            }
            match next {
                Ok(Msg::Rpc(v)) => {
                    if let Some(pos) = v.get("id").and_then(|id| cancelled.iter().position(|c| c == id)) {
                        cancelled.swap_remove(pos);
                        out.push(error(&v["id"], -32800, "request cancelled"));
                    } else {
                        self.handle(v, &mut out);
                    }
                }
                Ok(Msg::Fs(p)) => {
                    if let Some(p) = p.to_str() {
                        let mut p = p.to_owned();
                        normalize_path(&mut p);
                        self.fs_dirty.insert(p);
                    }
                }
                Ok(Msg::Indexed(idx, ms)) => {
                    self.install_index(*idx, ms);
                    self.building = None;
                    if std::mem::take(&mut self.progress_started) {
                        out.push(json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": PROGRESS_TOKEN, "value": {"kind": "end", "message": "done"}}}));
                    }
                    // Buffers opened while the build ran.
                    self.apply_pending();
                    if let Some(idx) = self.index.as_mut() {
                        for d in self.docs.values() {
                            idx.warm(&d.path, &d.text);
                        }
                    }
                }
                Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => return i32::from(!self.shutdown),
                Err(RecvTimeoutError::Timeout) => self.apply_pending(),
            }
            for m in out.drain(..) {
                if write_frame(&mut output, &m).is_err() {
                    return 1;
                }
            }
            if let Some(code) = self.exit_code {
                return code;
            }
        }
    }

    /// Publishes build progress (`x/y` in the status bar) at most every 100 ms.
    fn tick_progress(&mut self, out: &mut Vec<Value>) {
        let Some(p) = self.building.as_ref().filter(|_| self.progress_started) else { return };
        let snap = p.snapshot();
        if snap == self.last_progress.0 || self.last_progress.1.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_progress = (snap, std::time::Instant::now());
        out.push(json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": PROGRESS_TOKEN, "value": {
            "kind": "report",
            "message": format!("{}/{}", snap.0, snap.1),
            "percentage": if snap.1 > 0 { u64::from(snap.0) * 100 / u64::from(snap.1) } else { 0 },
        }}}));
    }

    fn install_index(&mut self, idx: Index, ms: f64) {
        eprintln!("lightspeed: indexed {} files, {} symbols in {:.0} ms", idx.live_files().count(), idx.db.len(), ms);
        eprintln!("{}", idx.stats.report(rayon::current_num_threads()));
        eprintln!("{}", idx.memory_report());
        self.index = Some(idx);
    }

    /// Returns a user-facing warning when no usable compile database was found.
    fn start_index(&mut self, root: Option<&str>) -> Option<String> {
        let mut warning = None;
        self.root = root.map(|r| r.replace('\\', "/").trim_end_matches('/').to_owned());
        let found = |root: Option<&str>| {
            let root = Path::new(root?);
            ["compile_commands.json", "build/compile_commands.json", "out/compile_commands.json"]
                .iter()
                .map(|r| root.join(r))
                .find(|p| p.is_file())
        };
        let cdb_path = match self.cdb_hint.clone() {
            Some(h) if h.is_file() => Some(h),
            Some(h) => {
                warning = Some(format!("lightspeed: compile database {} not found; searching the workspace", h.display()));
                found(root)
            }
            None => found(root),
        };
        if cdb_path.is_none() {
            warning = Some("lightspeed: no compile_commands.json found (workspace root, build/, out/); nothing indexed".into());
        }
        if let Some(w) = &warning {
            eprintln!("{w}");
        }
        let load = |p: Option<PathBuf>| match p.as_ref().map(CompileDb::load) {
            Some(Ok(mut db)) => {
                let n = db.add_system_includes();
                eprintln!("lightspeed: {n} built-in include dirs discovered");
                db
            }
            Some(Err(e)) => {
                eprintln!("lightspeed: {e}");
                CompileDb::default()
            }
            None => CompileDb::default(),
        };
        let t0 = std::time::Instant::now();
        let cfg = IndexConfig { cache: cdb_path.as_deref().and_then(crate::store::CacheConfig::for_project), ..IndexConfig::default() };
        match self.events.clone().filter(|_| self.background) {
            Some(tx) => {
                let progress = Arc::new(Progress::default());
                self.building = Some(progress.clone());
                // Parsing a huge compile database must not delay the `initialize` reply.
                let spawned = std::thread::Builder::new().name("lightspeed-index".into()).stack_size(16 << 20).spawn(move || {
                    let cdb = load(cdb_path);
                    let idx = Index::build_with(cdb, cfg, Some(&progress));
                    let _ = tx.send(Msg::Indexed(Box::new(idx), t0.elapsed().as_secs_f64() * 1e3));
                });
                if let Err(e) = spawned {
                    eprintln!("lightspeed: cannot start indexing thread: {e}");
                    self.building = None;
                }
            }
            None => {
                let idx = Index::build(load(cdb_path), cfg);
                self.install_index(idx, t0.elapsed().as_secs_f64() * 1e3);
            }
        }
        if let (Some(root), Some(tx)) = (root, self.events.clone()) {
            use notify::{EventKind, RecursiveMode, Watcher};
            let w = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
                if let Ok(ev) = ev
                    && matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_))
                {
                    for p in ev.paths.into_iter().filter(|p| is_source_ext(p)) {
                        let _ = tx.send(Msg::Fs(p));
                    }
                }
            });
            if let Ok(mut w) = w
                && w.watch(Path::new(root), RecursiveMode::Recursive).is_ok()
            {
                self._watcher = Some(w);
            }
        }
        warning
    }

    /// Applies buffered edits and disk changes to the index.
    pub fn apply_pending(&mut self) {
        let Some(idx) = self.index.as_mut() else { return };
        for d in self.docs.values_mut().filter(|d| d.dirty) {
            d.dirty = false;
            let unchanged = idx.file_index(&d.path).is_some_and(|i| idx.files[i].content_hash == hash_bytes(d.text.as_bytes()));
            if !unchanged {
                idx.update(&d.path, Some(&d.text));
            }
        }
        for p in std::mem::take(&mut self.fs_dirty) {
            // The editor's buffer is the truth for open files.
            if !self.docs.contains_key(&path_key(&p)) {
                idx.update(&p, None);
            }
        }
        idx.flush();
    }

    fn text_of(&self, path: &str) -> Option<Text> {
        Overlay(&self.docs).text(path)
    }

    /// Handles one message and logs its latency to stderr (shown in the
    /// editor's output panel). Set `LIGHTSPEED_QUIET` to silence it.
    pub fn handle(&mut self, msg: Value, out: &mut Vec<Value>) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let is_request = msg.get("id").is_some();
        let t0 = std::time::Instant::now();
        self.handle_inner(msg, out);
        let us = t0.elapsed().as_micros();
        if let Some(m) = method
            && (is_request || us >= 1000)
            && std::env::var_os("LIGHTSPEED_QUIET").is_none()
        {
            eprintln!("lightspeed: {m} {}.{:02} ms", us / 1000, us % 1000 / 10);
        }
    }

    fn handle_inner(&mut self, msg: Value, out: &mut Vec<Value>) {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else { return };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            ("initialize", Some(id)) => {
                let root = params
                    .get("rootUri")
                    .and_then(Value::as_str)
                    .and_then(uri_to_path)
                    .or_else(|| params.get("rootPath").and_then(Value::as_str).map(str::to_owned))
                    .or_else(|| {
                        params.pointer("/workspaceFolders/0/uri").and_then(Value::as_str).and_then(uri_to_path)
                    });
                // Index before replying so the first request never races the build
                // (unless indexing runs in the background, see `Server::background`).
                self.progress_ok = params["capabilities"]["window"]["workDoneProgress"].as_bool() == Some(true);
                self.apply_settings(&params["initializationOptions"]);
                let warning = self.start_index(root.as_deref());
                if let Some(w) = warning {
                    out.push(json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": w}}));
                }
                out.push(respond(
                    &id,
                    json!({
                        "capabilities": {
                            "positionEncoding": "utf-16",
                            "textDocumentSync": {"openClose": true, "change": 1, "save": {}},
                            "definitionProvider": true,
                            "hoverProvider": true,
                            "typeHierarchyProvider": true,
                            "renameProvider": {"prepareProvider": true},
                            "referencesProvider": true,
                            "completionProvider": {"triggerCharacters": [".", ":", ">"]},
                            "signatureHelpProvider": {"triggerCharacters": ["(", ","], "retriggerCharacters": [","]},
                            "documentSymbolProvider": true,
                            "workspaceSymbolProvider": true,
                        },
                        "serverInfo": {"name": "lightspeed", "version": env!("CARGO_PKG_VERSION")},
                    }),
                ));
            }
            ("shutdown", Some(id)) => {
                self.shutdown = true;
                out.push(respond(&id, Value::Null));
            }
            ("exit", _) => self.exit_code = Some(i32::from(!self.shutdown)),
            ("initialized", _) => {
                if self.building.is_some() && self.progress_ok {
                    self.progress_started = true;
                    out.push(json!({"jsonrpc": "2.0", "id": PROGRESS_TOKEN, "method": "window/workDoneProgress/create", "params": {"token": PROGRESS_TOKEN}}));
                    out.push(json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": PROGRESS_TOKEN, "value": {
                        "kind": "begin", "title": "Lightspeed indexing", "message": "starting", "percentage": 0,
                    }}}));
                }
            }
            ("textDocument/didOpen", _) => {
                let td = &params["textDocument"];
                if let (Some(uri), Some(text)) = (td["uri"].as_str(), td["text"].as_str())
                    && let Some(path) = uri_to_path(uri)
                {
                    let key = path_key(&path);
                    self.docs.insert(key, Doc { path, text: text.into(), dirty: true });
                    // Index on open so the first query doesn't pay for it; a no-op when the content is already indexed.
                    self.apply_pending();
                    if let Some(idx) = self.index.as_mut()
                        && let Some(d) = self.docs.get(&key)
                    {
                        idx.warm(&d.path, &d.text);
                    }
                }
            }
            ("textDocument/didChange", _) => {
                let uri = params["textDocument"]["uri"].as_str().and_then(uri_to_path);
                // Full sync: the last change carries the whole document.
                let text = params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str());
                if let (Some(path), Some(text)) = (uri, text) {
                    self.docs.insert(path_key(&path), Doc { path, text: text.into(), dirty: true });
                }
            }
            ("workspace/didChangeConfiguration", _) => self.apply_settings(&params["settings"]),
            ("textDocument/didClose", _) => {
                if let Some(path) = params["textDocument"]["uri"].as_str().and_then(uri_to_path)
                    && self.docs.remove(&path_key(&path)).is_some()
                {
                    self.fs_dirty.insert(path);
                }
            }
            (m, Some(id)) => {
                if self.index.is_none() {
                    // While the index builds, answer with an empty result instead of an error popup.
                    let result = if self.building.is_some() {
                        match m {
                            "textDocument/definition" | "textDocument/hover" | "textDocument/prepareRename" | "textDocument/signatureHelp" | "textDocument/prepareTypeHierarchy" | "typeHierarchy/supertypes" | "typeHierarchy/subtypes" | "lightspeed/inheritanceGraph" => Some(Value::Null),
                            "textDocument/references" | "textDocument/documentSymbol" | "workspace/symbol" => Some(json!([])),
                            "textDocument/completion" => Some(json!({"isIncomplete": true, "items": []})),
                            "textDocument/rename" => {
                                out.push(error(&id, -32803, "Lightspeed is still indexing; try again in a moment"));
                                return;
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    out.push(match result {
                        Some(r) => respond(&id, r),
                        None => error(&id, -32002, "server not initialized"),
                    });
                    return;
                }
                self.apply_pending();
                let result = match m {
                    "textDocument/definition" => self.definition(&params),
                    "textDocument/hover" => self.hover(&params),
                    "textDocument/signatureHelp" => self.signature_help(&params),
                    "textDocument/prepareTypeHierarchy" => self.prepare_type_hierarchy(&params),
                    "typeHierarchy/supertypes" => self.type_hierarchy_step(&params, false),
                    "typeHierarchy/subtypes" => self.type_hierarchy_step(&params, true),
                    "lightspeed/inheritanceGraph" => self.inheritance_graph(&params),
                    "textDocument/prepareRename" => self.prepare_rename(&params),
                    "textDocument/rename" => match self.rename(&params) {
                        Ok((edit, warning)) => {
                            if let Some(w) = warning {
                                out.push(json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": w}}));
                            }
                            Some(edit)
                        }
                        Err(msg) => {
                            out.push(error(&id, -32803, &msg));
                            return;
                        }
                    },
                    "textDocument/references" => self.references(&params),
                    "textDocument/completion" => self.completion(&params),
                    "textDocument/documentSymbol" => self.document_symbols(&params),
                    "workspace/symbol" => self.workspace_symbols(&params),
                    _ => {
                        out.push(error(&id, -32601, "method not found"));
                        return;
                    }
                };
                out.push(respond(&id, result.unwrap_or(Value::Null)));
            }
            _ => {}
        }
    }

    /// Document path, its text and the byte offset of the request position.
    fn cursor(&self, params: &Value) -> Option<(String, Text, u32)> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?)?;
        let text = self.text_of(&path)?;
        let (line, col) = (params["position"]["line"].as_u64()? as u32, params["position"]["character"].as_u64()? as u32);
        let li = LineIndex::new(text.as_str());
        let off = position_to_offset(text.as_str(), &li, line, col);
        Some((path, text, off))
    }

    fn locations_json(&self, locs: &[Location]) -> Value {
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

    fn range_json(text: &str, li: &LineIndex, (a, b): (u32, u32)) -> Value {
        let (s, e) = (offset_to_position(text, li, a), offset_to_position(text, li, b));
        json!({"start": {"line": s.0, "character": s.1}, "end": {"line": e.0, "character": e.1}})
    }

    /// Parameter/local at the request position: its occurrences as ranges in the same document.
    fn local_ranges(&self, params: &Value) -> Option<(Value, Vec<Value>, String)> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let l = Query::new(idx, &overlay).local_at(&path, text.as_str(), off)?;
        let li = LineIndex::new(text.as_str());
        let uses = l.uses.iter().map(|&r| Self::range_json(text.as_str(), &li, r)).collect();
        Some((Self::range_json(text.as_str(), &li, l.hovered), uses, l.name))
    }

    fn inside_root(&self, path: &str) -> bool {
        let Some(root) = &self.root else { return true };
        let (p, r) = (path.replace('\\', "/").to_lowercase(), root.to_lowercase());
        p.strip_prefix(&r).is_some_and(|rest| rest.starts_with('/'))
    }

    fn prepare_rename(&self, params: &Value) -> Option<Value> {
        if let Some((range, _, name)) = self.local_ranges(params) {
            return Some(json!({"range": range, "placeholder": name}));
        }
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
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
    fn rename(&self, params: &Value) -> Result<(Value, Option<String>), String> {
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
        let overlay = Overlay(&self.docs);
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

    fn definition(&self, params: &Value) -> Option<Value> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let q = Query::new(idx, &overlay);
        if let Some(l) = q.local_at(&path, text.as_str(), off) {
            let uri = params["textDocument"]["uri"].as_str()?;
            let range = Self::range_json(text.as_str(), &LineIndex::new(text.as_str()), l.decl);
            return Some(json!([{"uri": uri, "range": range}]));
        }
        let locs: Vec<Location> = q.definition(&path, text.as_str(), off).into_iter().filter_map(|s| q.location(s)).collect();
        Some(self.locations_json(&locs))
    }

    fn signature_help(&self, params: &Value) -> Option<Value> {
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let h = Query::new(idx, &overlay).signature_help(&path, text.as_str(), off, self.max_signatures)?;
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

    fn hover(&self, params: &Value) -> Option<Value> {
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
        if let Some(t) = self.text_of(&h.path) {
            let (line, _) = offset_to_position(t.as_str(), &LineIndex::new(t.as_str()), h.offset);
            let file = h.path.rsplit('/').next().unwrap_or(&h.path);
            md.push_str(&format!("\n\n---\n`{file}:{}`", line + 1));
            if h.more > 0 {
                md.push_str(&format!(" (+{} other candidate{})", h.more, if h.more == 1 { "" } else { "s" }));
            }
        }
        let li = LineIndex::new(text.as_str());
        let (s, e) = (offset_to_position(text.as_str(), &li, h.start), offset_to_position(text.as_str(), &li, h.end));
        Some(json!({
            "contents": {"kind": "markdown", "value": md},
            "range": {"start": {"line": s.0, "character": s.1}, "end": {"line": e.0, "character": e.1}},
        }))
    }

    fn references(&self, params: &Value) -> Option<Value> {
        if let Some((_, uses, _)) = self.local_ranges(params) {
            let uri = params["textDocument"]["uri"].as_str()?;
            return Some(Value::Array(uses.into_iter().map(|r| json!({"uri": uri, "range": r})).collect()));
        }
        let (path, text, off) = self.cursor(params)?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let q = Query::new(idx, &overlay);
        if let Some(r) = q.symbol_refs(&path, text.as_str(), off) {
            let mut locs = r.certain;
            locs.extend(r.uncertain);
            locs.sort_unstable();
            return Some(self.locations_json(&locs));
        }
        let (a, b) = identifier_at(text.as_str(), off)?;
        let name = &text.as_str()[a as usize..b as usize];
        let locs = q.references(name);
        Some(self.locations_json(&locs))
    }

    fn completion(&self, params: &Value) -> Option<Value> {
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

    /// `TypeHierarchyItem`s (also the nodes of the inheritance graph) for `ids`.
    fn type_items(&self, ids: &[SymbolId]) -> Vec<Value> {
        let Some(idx) = self.index.as_ref() else { return Vec::new() };
        let overlay = Overlay(&self.docs);
        let q = Query::new(idx, &overlay);
        let mut cache: HashMap<u32, Option<(Text, LineIndex, Value)>> = HashMap::new();
        let mut out = Vec::with_capacity(ids.len());
        for &s in ids {
            let Some(loc) = q.location(s) else { continue };
            let entry = cache.entry(loc.file).or_insert_with(|| {
                let path = &idx.files[loc.file as usize].path;
                let t = self.text_of(path)?;
                let li = LineIndex::new(t.as_str());
                Some((t, li, Value::String(path_to_uri(path))))
            });
            let Some((t, li, uri)) = entry else { continue };
            let sym = idx.db.symbol(s);
            let range = Self::range_json(t.as_str(), li, (loc.start, loc.end));
            out.push(json!({
                "name": idx.db.name(s),
                "kind": lsp_symbol_kind(sym.kind),
                "detail": idx.db.qualified_name(s),
                "uri": uri,
                "range": range,
                "selectionRange": range,
                "data": {"id": s.index()},
            }));
        }
        out
    }

    /// The class a hierarchy request refers to: a `data.id` item, a `typeName`
    /// (qualified or not), or the type at `textDocument` + `position`.
    fn hierarchy_class(&self, params: &Value) -> Option<SymbolId> {
        let idx = self.index.as_ref()?;
        let db = &idx.db;
        let item = params.get("item").unwrap_or(params);
        let overlay = Overlay(&self.docs);
        // Items are resolved by where the name is written now: symbol ids are
        // renumbered when the index is rebuilt after an edit, positions are not.
        if let (Some(uri), Some(line), Some(col)) = (item["uri"].as_str(), item["selectionRange"]["start"]["line"].as_u64(), item["selectionRange"]["start"]["character"].as_u64())
            && let Some(path) = uri_to_path(uri)
            && let Some(text) = self.text_of(&path)
        {
            let off = position_to_offset(text.as_str(), &LineIndex::new(text.as_str()), line as u32, col as u32);
            if let Some(id) = Query::new(idx, &overlay).class_at(&path, text.as_str(), off) {
                return Some(id);
            }
        }
        if let Some(raw) = item["data"]["id"].as_u64().or_else(|| params["id"].as_u64()) {
            let raw = raw as usize;
            return (raw < db.len() && db.symbol(SymbolId::from_usize(raw)).kind.is_record()).then(|| SymbolId::from_usize(raw));
        }
        if let Some(name) = params["typeName"].as_str() {
            let last = name.rsplit("::").next()?;
            return db.lookup(last).iter().copied().find(|&c| db.symbol(c).kind.is_record() && db.qualified_name(c).ends_with(name));
        }
        let (path, text, off) = self.cursor(params)?;
        Query::new(idx, &overlay).class_at(&path, text.as_str(), off)
    }

    fn prepare_type_hierarchy(&self, params: &Value) -> Option<Value> {
        let id = self.hierarchy_class(params)?;
        Some(Value::Array(self.type_items(&[id])))
    }

    fn type_hierarchy_step(&self, params: &Value, down: bool) -> Option<Value> {
        let id = self.hierarchy_class(params)?;
        let hier = &self.index.as_ref()?.hier;
        let ids: Vec<SymbolId> = if down { hier.derived(id) } else { hier.bases(id) }.to_vec();
        Some(Value::Array(self.type_items(&ids)))
    }

    /// Custom request `lightspeed/inheritanceGraph`: the class's ancestors and
    /// descendants as nodes and `{derived, base}` edges, for a graph view.
    /// Optional `maxDepth` (default 8) and `maxNodes` (default 300) bound the walk.
    fn inheritance_graph(&self, params: &Value) -> Option<Value> {
        let root = self.hierarchy_class(params)?;
        let hier = &self.index.as_ref()?.hier;
        let max_depth = params["maxDepth"].as_u64().unwrap_or(8) as usize;
        let max_nodes = params["maxNodes"].as_u64().unwrap_or(300) as usize;
        let mut nodes = vec![root];
        let mut seen: HashSet<SymbolId> = HashSet::new();
        seen.insert(root);
        let mut edges: Vec<(SymbolId, SymbolId)> = Vec::new();
        let mut truncated = false;
        for down in [false, true] {
            let mut level = vec![root];
            for _ in 0..max_depth {
                let mut next = Vec::new();
                for &c in &level {
                    for &n in if down { hier.derived(c) } else { hier.bases(c) } {
                        let (derived, base) = if down { (n, c) } else { (c, n) };
                        if seen.insert(n) {
                            if nodes.len() >= max_nodes {
                                truncated = true;
                                seen.remove(&n);
                                continue;
                            }
                            nodes.push(n);
                            next.push(n);
                        }
                        if seen.contains(&n) {
                            edges.push((derived, base));
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                level = next;
            }
        }
        let items = self.type_items(&nodes);
        let edges: Vec<Value> = edges.iter().map(|&(d, b)| json!({"derived": d.index(), "base": b.index()})).collect();
        Some(json!({"root": root.index(), "nodes": items, "edges": edges, "truncated": truncated}))
    }

    /// `SymbolInformation` for each symbol; files are loaded and line-indexed once per call.
    fn symbol_infos(&self, ids: impl IntoIterator<Item = SymbolId>) -> Value {
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

    fn document_symbols(&self, params: &Value) -> Option<Value> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?)?;
        let idx = self.index.as_ref()?;
        let fi = idx.file_index(&path)?;
        Some(self.symbol_infos(idx.file_symbols(fi)))
    }

    fn workspace_symbols(&self, params: &Value) -> Option<Value> {
        let q = params["query"].as_str()?;
        let idx = self.index.as_ref()?;
        let overlay = Overlay(&self.docs);
        let hits = Query::new(idx, &overlay).search_symbols(q, WORKSPACE_SYMBOL_LIMIT);
        Some(self.symbol_infos(hits))
    }
}

/// Convenience for the CLI: serve on the process's stdio.
pub fn serve_stdio(cdb: Option<PathBuf>) -> i32 {
    let stdin = std::io::BufReader::new(std::io::stdin());
    Server::new(cdb).run(stdin, std::io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_indexing_reports_progress_and_delivers_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().replace('\\', "/");
        std::fs::write(format!("{root}/a.cpp"), "struct A {};\n").unwrap();
        std::fs::write(
            format!("{root}/compile_commands.json"),
            serde_json::to_string(&json!([{"directory": root, "file": "a.cpp", "arguments": ["clang++", "-c", "a.cpp"]}])).unwrap(),
        )
        .unwrap();
        let (tx, rx) = channel();
        let mut s = Server::new(None);
        s.background = true;
        s.events = Some(tx);
        let mut out = Vec::new();
        let params = json!({"rootUri": path_to_uri(&root), "capabilities": {"window": {"workDoneProgress": true}}});
        s.handle(json!({"id": 1, "method": "initialize", "params": params}), &mut out);
        assert!(s.index().is_none() && s.building.is_some());
        out.clear();
        s.handle(json!({"method": "initialized", "params": {}}), &mut out);
        assert_eq!(out[0]["method"], "window/workDoneProgress/create");
        assert_eq!(out[1]["params"]["value"]["kind"], "begin");
        loop {
            match rx.recv_timeout(Duration::from_secs(30)).unwrap() {
                Msg::Indexed(idx, _) => {
                    assert!(idx.live_files().count() >= 1);
                    break;
                }
                _ => continue,
            }
        }
        let (done, total) = s.building.as_ref().unwrap().snapshot();
        assert!(done >= 1 && done <= total, "{done}/{total}");
    }
}
