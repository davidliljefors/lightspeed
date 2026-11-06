//! The running server: the message loop, background indexing, the file watcher and applying edits to the index.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use serde_json::{Value, json};

use super::transport::{read_frame, write_frame, error};
use super::{Msg, PROGRESS_TOKEN, Server};
use crate::workspace::compile_commands::CompileDb;
use crate::base::hash::hash_bytes;
use crate::workspace::index::{Index, IndexConfig, Progress};
use crate::pp::vfs::{normalize_path, path_key};

const IDLE_FLUSH: Duration = Duration::from_millis(150);

pub(super) fn is_source_ext(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(e.to_ascii_lowercase().as_str(), "c" | "cc" | "cpp" | "cxx" | "c++" | "h" | "hh" | "hpp" | "hxx" | "inl" | "ipp" | "tpp")
    })
}

impl Server {
    /// Serves until the client exits or closes stdin. Returns the exit code.
    pub fn run(mut self, input: impl BufRead + Send + 'static, output: impl Write + Send + 'static) -> i32 {
        let output = Arc::new(std::sync::Mutex::new(output));
        let live = output.clone();
        // Batches of a long answer leave from the thread that found them, ahead of the final response.
        self.set_sink(move |m| {
            let _ = write_frame(&mut *live.lock().unwrap_or_else(|e| e.into_inner()), &m);
        });
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
                    self.refresh_tokens(&mut out);
                    if let Some(idx) = self.index.as_mut() {
                        for d in self.docs.values() {
                            idx.warm(&d.path, &d.text);
                        }
                    }
                }
                Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => return i32::from(!self.shutdown),
                Err(RecvTimeoutError::Timeout) => {
                    if self.apply_pending() {
                        self.refresh_tokens(&mut out);
                    } else if let Some(idx) = self.index.as_mut()
                        && idx.maintain()
                    {
                        crate::log!("lightspeed: compacted the index while idle");
                    }
                }
            }
            for m in out.drain(..) {
                if write_frame(&mut *output.lock().unwrap_or_else(|e| e.into_inner()), &m).is_err() {
                    return 1;
                }
            }
            if let Some(code) = self.exit_code {
                return code;
            }
        }
    }

    /// Publishes build progress (`x/y` in the status bar) at most every 100 ms.
    pub(in crate::lsp) fn tick_progress(&mut self, out: &mut Vec<Value>) {
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

    pub(in crate::lsp) fn install_index(&mut self, idx: Index, ms: f64) {
        crate::log!("lightspeed: indexed {} files, {} symbols in {:.0} ms", idx.live_files().count(), idx.db.len(), ms);
        crate::log!("{}", idx.stats.report(rayon::current_num_threads()));
        crate::log!("{}", idx.memory_report());
        let (store, paths): (_, Vec<String>) = (idx.texts.clone(), idx.live_files().map(|(_, f)| f.path.clone()).collect());
        self.index = Some(idx);
        // Every file is needed sooner or later; compress them all in the background.
        let _ = std::thread::Builder::new().name("lightspeed-texts".into()).spawn(move || {
            let t = std::time::Instant::now();
            store.warm(&paths);
            let (n, raw, packed) = store.stats();
            crate::log!("lightspeed: cached {n} files, {} MB as {} MB in {:.1} s", raw >> 20, packed >> 20, t.elapsed().as_secs_f64());
        });
    }

    /// Returns a user-facing warning when no usable compile database was found.
    pub(in crate::lsp) fn start_index(&mut self, root: Option<&str>) -> Option<String> {
        let mut warning = None;
        self.root = root.map(|r| r.replace('\\', "/").trim_end_matches('/').to_owned());
        let found = |root: Option<&str>| {
            let root = Path::new(root?);
            ["compile_commands.json", "build/compile_commands.json", "out/compile_commands.json"]
                .iter()
                .map(|r| root.join(r))
                .find(|p| p.is_file())
        };
        let use_ninja = self.ninja.is_some();
        let cdb_path = match (self.ninja.clone(), self.cdb_hint.clone()) {
            (Some(n), _) if n.is_file() => Some(n),
            (Some(n), _) => {
                warning = Some(format!("lightspeed: ninja file {} not found; nothing indexed", n.display()));
                None
            }
            (None, Some(h)) if h.is_file() => Some(h),
            (None, Some(h)) => {
                warning = Some(format!("lightspeed: compile database {} not found; searching the workspace", h.display()));
                found(root)
            }
            (None, None) => found(root),
        };
        if cdb_path.is_none() && !use_ninja {
            warning = Some("lightspeed: no compile_commands.json found (workspace root, build/, out/); nothing indexed".into());
        }
        if let Some(w) = &warning {
            eprintln!("{w}");
        }
        let load = move |p: Option<PathBuf>| match p.as_ref().map(|p| if use_ninja { CompileDb::load_ninja(p) } else { CompileDb::load(p) }) {
            Some(Ok(mut db)) => {
                let n = db.add_system_includes();
                crate::log!("lightspeed: {n} built-in include dirs discovered");
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

    /// Applies buffered edits and disk changes to the index; true if anything changed.
    pub fn apply_pending(&mut self) -> bool {
        self.apply(false)
    }

    /// With `defer`, edits to buffers the index already knows are left pending: the caller answers
    /// from the index as it is and maps positions through the edit (see `stale`).
    pub(in crate::lsp) fn apply(&mut self, defer: bool) -> bool {
        let Some(idx) = self.index.as_mut() else { return false };
        let mut changed = false;
        for d in self.docs.values_mut().filter(|d| d.dirty) {
            if defer && d.indexed.is_some() {
                continue;
            }
            d.dirty = false;
            let unchanged = idx.file_index(&d.path).is_some_and(|i| idx.files[i].content_hash == hash_bytes(d.text.as_bytes()));
            if !unchanged {
                idx.update(&d.path, Some(&d.text));
                changed = true;
            }
            d.indexed = Some(d.text.clone());
        }
        for p in std::mem::take(&mut self.fs_dirty) {
            // The editor's buffer is the truth for open files.
            if !self.docs.contains_key(&path_key(&p)) {
                idx.update(&p, None);
                changed = true;
            }
        }
        idx.flush();
        changed
    }

    /// Asks the client to re-request semantic tokens: classifications depend on the whole index.
    pub(in crate::lsp) fn refresh_tokens(&mut self, out: &mut Vec<Value>) {
        if self.caps.token_refresh {
            self.refresh_seq += 1;
            out.push(json!({"jsonrpc": "2.0", "id": format!("lightspeed-refresh-{}", self.refresh_seq), "method": "workspace/semanticTokens/refresh"}));
        }
    }
}
