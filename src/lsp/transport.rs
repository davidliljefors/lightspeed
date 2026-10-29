//! JSON-RPC framing (`Content-Length` headers) and reply constructors.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use super::Server;

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

pub(super) fn respond(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(super) fn error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Convenience for the CLI: serve on the process's stdio.
pub fn serve_stdio(cdb: Option<PathBuf>, certain_only: bool) -> i32 {
    let stdin = std::io::BufReader::new(std::io::stdin());
    let mut server = Server::new(cdb);
    server.set_certain_only(certain_only);
    server.run(stdin, std::io::stdout().lock())
}
