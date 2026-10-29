//! Partial results (LSP 3.15 `partialResultToken`): a request's answer is sent as `$/progress`
//! batches while it is still being computed. The final response is then empty, so a client
//! that combines batches and response never sees an item twice.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Writes one message to the client right away, from any thread.
pub(super) type Sink = Arc<dyn Fn(Value) + Send + Sync>;

/// Items held back until a batch is worth a message.
const BATCH: usize = 64;
/// A held batch is sent anyway once this much time passed since the last one.
const MAX_HOLD: Duration = Duration::from_millis(30);

/// Methods whose result is a list the client can accept in pieces.
const STREAMING: &[&str] = &[
    "textDocument/definition",
    "textDocument/declaration",
    "textDocument/implementation",
    "textDocument/typeDefinition",
    "textDocument/references",
    "textDocument/documentSymbol",
    "workspace/symbol",
    "typeHierarchy/supertypes",
    "typeHierarchy/subtypes",
];

pub(super) struct Partial {
    token: Value,
    sink: Option<Sink>,
    state: Mutex<State>,
    sent: AtomicBool,
}

struct State {
    held: Vec<Value>,
    last: Instant,
    /// Messages produced when there is no live sink; the caller appends them before the response.
    queued: Vec<Value>,
}

impl Partial {
    /// A collector when streaming is on, `method` supports it and the client sent a token.
    pub(super) fn for_request(enabled: bool, method: &str, params: &Value, sink: Option<Sink>) -> Option<Arc<Self>> {
        let token = params.get("partialResultToken").filter(|t| !t.is_null())?;
        if !enabled || !STREAMING.contains(&method) {
            return None;
        }
        Some(Arc::new(Self {
            token: token.clone(),
            sink,
            state: Mutex::new(State { held: Vec::new(), last: Instant::now(), queued: Vec::new() }),
            sent: AtomicBool::new(false),
        }))
    }

    /// Adds items; sends batches while enough piled up, or what is held once it has waited long enough.
    pub(super) fn push(&self, items: impl IntoIterator<Item = Value>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.held.extend(items);
        while st.held.len() >= BATCH {
            let rest = st.held.split_off(BATCH);
            self.send_held(&mut st);
            st.held = rest;
        }
        if !st.held.is_empty() && st.last.elapsed() >= MAX_HOLD {
            self.send_held(&mut st);
        }
    }

    fn send_held(&self, st: &mut State) {
        let batch = std::mem::take(&mut st.held);
        st.last = Instant::now();
        let msg = json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": self.token, "value": batch}});
        self.sent.store(true, Ordering::Relaxed);
        match &self.sink {
            Some(sink) => sink(msg),
            None => st.queued.push(msg),
        }
    }

    /// Ends the request: `result` is the handler's own answer, items it pushed come on top.
    /// Returns the final response value and any queued messages to send before it.
    pub(super) fn finish(&self, result: Option<Value>) -> (Option<Value>, Vec<Value>) {
        let was_array = matches!(result, Some(Value::Array(_)));
        if let Some(Value::Array(items)) = result {
            if items.len() > BATCH || self.sent.load(Ordering::Relaxed) {
                self.push(items);
            } else if !items.is_empty() {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.held.splice(0..0, items);
            }
        } else if let Some(other) = result {
            return (Some(other), Vec::new());
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.sent.load(Ordering::Relaxed) {
            if !st.held.is_empty() {
                self.send_held(&mut st);
            }
            return (Some(json!([])), std::mem::take(&mut st.queued));
        }
        // Nothing was worth streaming: answer in the usual way.
        if st.held.is_empty() && !was_array {
            return (None, Vec::new());
        }
        (Some(Value::Array(std::mem::take(&mut st.held))), Vec::new())
    }
}
