//! Completion: scope, `Qual::`, and `receiver.` / `receiver->` member lists.

use super::{Query, Text, is_ident_byte, sig_tokens};
use crate::lexer::TokenKind;
use crate::symbols::{SymbolId, SymbolKind};
use rustc_hash::FxHashMap;

impl Query<'_> {
    /// Completion candidates for the cursor at `offset`. Returns symbols and
    /// whether the list was truncated.
    pub fn completions(&self, path: &str, text: &str, offset: u32, limit: usize) -> (Vec<SymbolId>, bool) {
        let b = text.as_bytes();
        let off = (offset as usize).min(b.len());
        let mut ps = off;
        while ps > 0 && is_ident_byte(b[ps - 1]) {
            ps -= 1;
        }
        let prefix = &text[ps..off];
        let mut k = ps;
        while k > 0 && b[k - 1].is_ascii_whitespace() {
            k -= 1;
        }
        let fi = self.idx.file_index(path);
        let scopes = self.scopes_at(fi, offset);
        let mut out: Vec<SymbolId> = Vec::new();
        let mut truncated = false;
        let db = &self.idx.db;
        let push_members = |out: &mut Vec<SymbolId>, owner: SymbolId, truncated: &mut bool| {
            let mut chain = vec![owner];
            if db.symbol(owner).kind.is_record() {
                self.idx.hier.all_bases(owner, &mut chain);
            }
            for o in chain {
                for &m in db.children(o) {
                    if db.name(m).starts_with(prefix) {
                        if out.len() >= limit {
                            *truncated = true;
                            return;
                        }
                        out.push(m);
                    }
                }
            }
        };

        if k >= 2 && &b[k - 2..k] == b"::" {
            let mut qual: Vec<&str> = Vec::new();
            let mut e = k - 2;
            loop {
                while e > 0 && b[e - 1].is_ascii_whitespace() {
                    e -= 1;
                }
                let mut s = e;
                while s > 0 && is_ident_byte(b[s - 1]) {
                    s -= 1;
                }
                if s == e {
                    break;
                }
                qual.push(&text[s..e]);
                if s >= 2 && &b[s - 2..s] == b"::" {
                    e = s - 2;
                } else {
                    break;
                }
            }
            qual.reverse();
            if let Some(&last) = qual.last() {
                let want = qual.join("::");
                let mut buf = String::new();
                for &c in db.lookup(last) {
                    let c = self.idx.hier.canonical(c);
                    if !db.symbol(c).kind.is_scope() {
                        continue;
                    }
                    buf.clear();
                    db.write_qualified_name(c, &mut buf);
                    if buf == want || buf.strip_suffix(want.as_str()).is_some_and(|r| r.ends_with("::")) {
                        push_members(&mut out, c, &mut truncated);
                    }
                }
            }
        } else if k >= 1 && (b[k - 1] == b'.' || (k >= 2 && &b[k - 2..k] == b"->")) {
            let recv_end = if b[k - 1] == b'.' { k - 1 } else { k - 2 };
            let types = self.receiver_types(path, text, recv_end as u32, &scopes);
            if types.is_empty() {
                // Unknown receiver: members of any class with a matching name.
                for (n, s) in db.names.iter() {
                    if !s.starts_with(prefix) {
                        continue;
                    }
                    for &m in db.lookup_id(n) {
                        if db.symbol(m).parent.is_some_and(|p| db.symbol(p).kind.is_record()) {
                            if out.len() >= limit {
                                truncated = true;
                                break;
                            }
                            out.push(m);
                        }
                    }
                    if truncated {
                        break;
                    }
                }
            } else {
                for t in types {
                    push_members(&mut out, t, &mut truncated);
                }
            }
        } else {
            let (in_scope, in_bases) = self.scope_set(&scopes);
            for s in in_scope.into_iter().chain(in_bases) {
                push_members(&mut out, s, &mut truncated);
            }
            if !prefix.is_empty() {
                'outer: for (n, s) in db.names.iter() {
                    if !s.starts_with(prefix) {
                        continue;
                    }
                    for &m in db.lookup_id(n) {
                        if db.symbol(m).parent.is_none_or(|p| db.symbol(p).kind == SymbolKind::Namespace) {
                            if out.len() >= limit {
                                truncated = true;
                                break 'outer;
                            }
                            out.push(m);
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        (out, truncated)
    }

    /// One-line declaration text for each symbol (`virtual Foo* get() const`,
    /// `int count`), for completion detail. Files are read once per call.
    pub fn summaries(&self, ids: &[SymbolId]) -> Vec<Option<String>> {
        let db = &self.idx.db;
        let mut files: FxHashMap<&str, Option<Text>> = FxHashMap::default();
        ids.iter()
            .map(|&id| {
                let s = db.symbol(id);
                let path = db.files.path(s.file);
                let text = files.entry(path).or_insert_with(|| self.texts.text(path));
                let all = text.as_ref()?.as_str();
                if s.kind == SymbolKind::Macro {
                    let line = all.get(s.name_offset as usize..)?.lines().next()?;
                    return Some(truncate(&format!("#define {}", line.trim())));
                }
                let src = all.get(s.extent_start as usize..s.extent_end as usize)?;
                summarize(src, s.kind)
            })
            .collect()
    }
}

const SUMMARY_MAX: usize = 120;

/// The declaration head of `src`: up to the body, initializer or base list.
fn summarize(src: &str, kind: SymbolKind) -> Option<String> {
    let toks = sig_tokens(src);
    let is_fn = matches!(kind, SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Destructor);
    let mut depth = 0i32;
    let mut end = toks.len();
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket => depth -= 1,
            TokenKind::Semi | TokenKind::LBrace if depth <= 0 => end = i,
            TokenKind::Colon | TokenKind::Eq if depth <= 0 && !(is_fn && t.kind == TokenKind::Eq) => end = i,
            _ => continue,
        }
        if end == i {
            break;
        }
    }
    let (first, last) = (toks.first()?, toks.get(end.checked_sub(1)?)?);
    let head = src.get(first.start as usize..last.end() as usize)?;
    let mut out = String::with_capacity(head.len().min(SUMMARY_MAX + 4));
    for w in head.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    Some(truncate(&out))
}

fn truncate(s: &str) -> String {
    match s.char_indices().nth(SUMMARY_MAX) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}
