use std::sync::mpsc::channel;
use std::time::Duration;

use serde_json::json;

use super::position::path_to_uri;
use super::{Msg, Server};

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
