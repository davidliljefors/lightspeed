//! URIs and positions: LSP speaks `file://` URIs and UTF-16 columns, the index speaks paths and byte offsets.

use crate::pp::vfs::normalize_path;
use crate::source::LineIndex;

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
