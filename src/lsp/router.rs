//! Message dispatch. Every request is one row in [`ROUTES`]: its method name, what to
//! answer while the index is still building, and the handler. Adding a request means
//! adding a row here, a handler in `handlers/`, and (for a standard feature) its
//! capability in [`capabilities`].

use serde_json::{Value, json};

use super::position::uri_to_path;
use super::transport::{error, respond};
use super::{Doc, PROGRESS_TOKEN, Server};
use crate::pp::vfs::path_key;

/// What a request answers while the index is still building (instead of an error popup).
#[derive(Clone, Copy)]
enum Pending {
    Null,
    EmptyList,
    EmptyCompletion,
    Busy,
}

/// `Err` becomes a JSON-RPC error (code -32803, "request failed").
type Handler = fn(&Server, &Value, &mut Vec<Value>) -> Result<Option<Value>, String>;

fn ok(v: Option<Value>) -> Result<Option<Value>, String> {
    Ok(v)
}

const ROUTES: &[(&str, Pending, Handler)] = &[
    ("textDocument/definition", Pending::Null, |s, p, _| ok(s.definition(p))),
    ("textDocument/declaration", Pending::Null, |s, p, _| ok(s.navigate(p, true, |q, p, t, o| q.declarations(p, t, o)))),
    ("textDocument/implementation", Pending::Null, |s, p, _| ok(s.navigate(p, false, |q, p, t, o| q.implementations(p, t, o)))),
    ("textDocument/typeDefinition", Pending::Null, |s, p, _| ok(s.navigate(p, false, |q, p, t, o| q.type_definitions(p, t, o)))),
    ("textDocument/references", Pending::EmptyList, |s, p, _| ok(s.references(p))),
    ("textDocument/hover", Pending::Null, |s, p, _| ok(s.hover(p))),
    ("textDocument/signatureHelp", Pending::Null, |s, p, _| ok(s.signature_help(p))),
    ("textDocument/completion", Pending::EmptyCompletion, |s, p, _| ok(s.completion(p))),
    ("textDocument/documentSymbol", Pending::EmptyList, |s, p, _| ok(s.document_symbols(p))),
    ("workspace/symbol", Pending::EmptyList, |s, p, _| ok(s.workspace_symbols(p))),
    ("textDocument/prepareRename", Pending::Null, |s, p, _| ok(s.prepare_rename(p))),
    ("textDocument/rename", Pending::Busy, |s, p, out| {
        let (edit, warning) = s.rename(p)?;
        if let Some(w) = warning {
            out.push(json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": w}}));
        }
        Ok(Some(edit))
    }),
    ("textDocument/prepareTypeHierarchy", Pending::Null, |s, p, _| ok(s.prepare_type_hierarchy(p))),
    ("typeHierarchy/supertypes", Pending::Null, |s, p, _| ok(s.type_hierarchy_step(p, false))),
    ("typeHierarchy/subtypes", Pending::Null, |s, p, _| ok(s.type_hierarchy_step(p, true))),
    ("lightspeed/inheritanceGraph", Pending::Null, |s, p, _| ok(s.inheritance_graph(p))),
    ("textDocument/semanticTokens/full", Pending::Null, |s, p, _| ok(s.semantic_tokens(p, false))),
    ("textDocument/semanticTokens/full/delta", Pending::Null, |s, p, _| ok(s.semantic_tokens_delta(p))),
    ("textDocument/semanticTokens/range", Pending::Null, |s, p, _| ok(s.semantic_tokens(p, true))),
];

fn capabilities() -> Value {
    json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": {"openClose": true, "change": 1, "save": {}},
        "definitionProvider": true,
        "declarationProvider": true,
        "implementationProvider": true,
        "typeDefinitionProvider": true,
        "hoverProvider": true,
        "typeHierarchyProvider": true,
        "renameProvider": {"prepareProvider": true},
        "referencesProvider": true,
        "completionProvider": {"triggerCharacters": [".", ":", ">"]},
        "signatureHelpProvider": {"triggerCharacters": ["(", ","], "retriggerCharacters": [","]},
        "documentSymbolProvider": true,
        "semanticTokensProvider": {
            "legend": {"tokenTypes": crate::query::SEMANTIC_TYPES, "tokenModifiers": crate::query::SEMANTIC_MODIFIERS},
            "full": {"delta": true},
            "range": true,
        },
        "workspaceSymbolProvider": true,
    })
}

impl Server {
    /// Handles one message and logs its latency to stderr (shown in the
    /// editor's output panel). Set `LIGHTSPEED_QUIET` to silence it.
    pub fn handle(&mut self, msg: Value, out: &mut Vec<Value>) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let is_request = msg.get("id").is_some();
        let t0 = std::time::Instant::now();
        self.dispatch(msg, out);
        let us = t0.elapsed().as_micros();
        if let Some(m) = method
            && (is_request || us >= 1000)
            && std::env::var_os("LIGHTSPEED_QUIET").is_none()
        {
            eprintln!("lightspeed: {m} {}.{:02} ms", us / 1000, us % 1000 / 10);
        }
    }

    fn dispatch(&mut self, msg: Value, out: &mut Vec<Value>) {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else { return };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            ("initialize", Some(id)) => self.initialize(&id, &params, out),
            ("shutdown", Some(id)) => {
                self.shutdown = true;
                out.push(respond(&id, Value::Null));
            }
            ("exit", _) => self.exit_code = Some(i32::from(!self.shutdown)),
            ("initialized", _) => self.initialized(out),
            ("textDocument/didOpen", _) => self.did_open(&params),
            ("textDocument/didChange", _) => self.did_change(&params),
            ("textDocument/didClose", _) => self.did_close(&params),
            ("workspace/didChangeConfiguration", _) => self.apply_settings(&params["settings"]),
            (m, Some(id)) => self.request(m, &id, &params, out),
            _ => {}
        }
    }

    fn request(&mut self, method: &str, id: &Value, params: &Value, out: &mut Vec<Value>) {
        let route = ROUTES.iter().find(|r| r.0 == method);
        if self.index.is_none() {
            let reply = match (self.building.is_some(), route) {
                (true, Some(&(_, Pending::Null, _))) => respond(id, Value::Null),
                (true, Some(&(_, Pending::EmptyList, _))) => respond(id, json!([])),
                (true, Some(&(_, Pending::EmptyCompletion, _))) => respond(id, json!({"isIncomplete": true, "items": []})),
                (true, Some(&(_, Pending::Busy, _))) => error(id, -32803, "Lightspeed is still indexing; try again in a moment"),
                _ => error(id, -32002, "server not initialized"),
            };
            out.push(reply);
            return;
        }
        self.apply_pending();
        let Some(&(_, _, handler)) = route else {
            out.push(error(id, -32601, "method not found"));
            return;
        };
        let reply = match handler(self, params, out) {
            Ok(result) => respond(id, result.unwrap_or(Value::Null)),
            Err(msg) => error(id, -32803, &msg),
        };
        out.push(reply);
    }

    fn initialize(&mut self, id: &Value, params: &Value, out: &mut Vec<Value>) {
        let root = params
            .get("rootUri")
            .and_then(Value::as_str)
            .and_then(uri_to_path)
            .or_else(|| params.get("rootPath").and_then(Value::as_str).map(str::to_owned))
            .or_else(|| params.pointer("/workspaceFolders/0/uri").and_then(Value::as_str).and_then(uri_to_path));
        // Index before replying so the first request never races the build
        // (unless indexing runs in the background, see `Server::background`).
        self.caps.progress = params["capabilities"]["window"]["workDoneProgress"].as_bool() == Some(true);
        self.caps.token_refresh = params["capabilities"]["workspace"]["semanticTokens"]["refreshSupport"].as_bool() == Some(true);
        self.apply_settings(&params["initializationOptions"]);
        if let Some(w) = self.start_index(root.as_deref()) {
            out.push(json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": w}}));
        }
        out.push(respond(
            id,
            json!({
                "capabilities": capabilities(),
                "serverInfo": {"name": "lightspeed", "version": env!("CARGO_PKG_VERSION")},
            }),
        ));
    }

    fn initialized(&mut self, out: &mut Vec<Value>) {
        if self.building.is_some() && self.caps.progress {
            self.progress_started = true;
            out.push(json!({"jsonrpc": "2.0", "id": PROGRESS_TOKEN, "method": "window/workDoneProgress/create", "params": {"token": PROGRESS_TOKEN}}));
            out.push(json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": PROGRESS_TOKEN, "value": {
                "kind": "begin", "title": "Lightspeed indexing", "message": "starting", "percentage": 0,
            }}}));
        }
    }

    fn did_open(&mut self, params: &Value) {
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

    fn did_change(&mut self, params: &Value) {
        let uri = params["textDocument"]["uri"].as_str().and_then(uri_to_path);
        // Full sync: the last change carries the whole document.
        let text = params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str());
        if let (Some(path), Some(text)) = (uri, text) {
            self.docs.insert(path_key(&path), Doc { path, text: text.into(), dirty: true });
        }
    }

    fn did_close(&mut self, params: &Value) {
        if let Some(path) = params["textDocument"]["uri"].as_str().and_then(uri_to_path) {
            self.sem.docs.lock().unwrap().remove(&path_key(&path));
            if self.docs.remove(&path_key(&path)).is_some() {
                self.fs_dirty.insert(path);
            }
        }
    }
}
