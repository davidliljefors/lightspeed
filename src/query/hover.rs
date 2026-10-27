//! Hover: what the symbol under the cursor resolved to, as plain data.

use super::{LocalKind, Query, identifier_at};
use crate::symbols::SymbolFlags;

pub struct Hover {
    /// Byte range of the hovered identifier.
    pub start: u32,
    pub end: u32,
    /// One-line declaration text.
    pub signature: String,
    pub kind: String,
    /// Qualified name of the enclosing scope, empty at global scope.
    pub container: String,
    /// Declaring file and the byte offset of the name in it.
    pub path: String,
    pub offset: u32,
    pub doc: Option<String>,
    /// Facts like `virtual`, family size, bases.
    pub notes: Vec<String>,
    /// Other candidates the name resolved to.
    pub more: usize,
}

const MAX_DOC_LINES: usize = 30;

impl Query<'_> {
    pub fn hover(&self, path: &str, text: &str, offset: u32) -> Option<Hover> {
        let (start, end) = identifier_at(text, offset)?;
        if let Some(l) = self.local_at(path, text, offset) {
            let mut notes = Vec::new();
            if l.auto
                && let Some(t) = self.local_type(path, text, &l.name, l.decl_offset)
            {
                notes.push(format!("type: {t}"));
            }
            let (db, f) = (&self.idx.db, l.function);
            return Some(Hover {
                start,
                end,
                signature: l.signature,
                kind: match l.kind {
                    LocalKind::Param => "parameter",
                    LocalKind::Variable => "local variable",
                    LocalKind::TemplateParam => "template parameter",
                }
                .to_owned(),
                container: db.qualified_name(self.idx.hier.canonical(f)),
                path: path.to_owned(),
                offset: l.decl.0,
                doc: None,
                notes,
                more: 0,
            });
        }
        let cands = self.definition(path, text, offset);
        let &id = cands.first()?;
        let (db, hier) = (&self.idx.db, &self.idx.hier);
        let s = db.symbol(id);
        let decl_path = db.files.path(s.file);
        let signature = self.summaries(&[id]).remove(0).unwrap_or_else(|| db.name(id).to_owned());
        let doc = self.texts.text(decl_path).and_then(|t| doc_comment(t.as_str(), s.extent_start as usize));

        let mut notes = Vec::new();
        let flag_names = [
            (SymbolFlags::VIRTUAL, "virtual"),
            (SymbolFlags::PURE, "pure"),
            (SymbolFlags::OVERRIDE, "override"),
            (SymbolFlags::FINAL, "final"),
            (SymbolFlags::STATIC, "static"),
            (SymbolFlags::DELETED, "deleted"),
        ];
        let flags: Vec<&str> = flag_names.iter().filter(|(f, _)| s.flags.contains(*f)).map(|&(_, n)| n).collect();
        if !flags.is_empty() {
            notes.push(flags.join(", "));
        }
        if let Some(f) = s.family {
            let n = hier.family(f).len();
            if n > 1 {
                notes.push(format!("virtual family of {n} methods"));
            }
        }
        if s.kind.is_record() {
            let mut bases = Vec::new();
            hier.all_bases(id, &mut bases);
            let names: Vec<&str> = bases.iter().filter(|&&b| b != id).map(|&b| db.name(b)).take(8).collect();
            if !names.is_empty() {
                notes.push(format!("bases: {}", names.join(", ")));
            }
            let derived = hier.derived(id).len();
            if derived > 0 {
                notes.push(format!("{derived} direct subclass{}", if derived == 1 { "" } else { "es" }));
            }
        }

        Some(Hover {
            start,
            end,
            signature,
            kind: format!("{:?}", s.kind).to_lowercase(),
            container: s.parent.map(|p| db.qualified_name(p)).unwrap_or_default(),
            path: decl_path.to_owned(),
            offset: s.name_offset,
            doc,
            notes,
            more: cands.len() - 1,
        })
    }
}

/// The `//` lines or `/* */` block directly above the declaration at `start`.
fn doc_comment(src: &str, start: usize) -> Option<String> {
    let before = src.get(..start)?;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let mut lines = src[..line_start].lines().rev().peekable();
    let mut out: Vec<&str> = Vec::new();
    if lines.peek()?.trim_end().ends_with("*/") {
        for l in lines {
            let t = l.trim();
            out.push(t.trim_start_matches("/**").trim_start_matches("/*").trim_end_matches("*/").trim_start_matches('*').trim());
            if t.contains("/*") || out.len() >= MAX_DOC_LINES {
                break;
            }
        }
    } else {
        for l in lines {
            let t = l.trim();
            let Some(body) = t.strip_prefix("//") else { break };
            out.push(body.trim_start_matches(['/', '!']).trim());
            if out.len() >= MAX_DOC_LINES {
                break;
            }
        }
    }
    out.reverse();
    let doc = out.join("\n").trim().to_owned();
    (!doc.is_empty()).then_some(doc)
}
